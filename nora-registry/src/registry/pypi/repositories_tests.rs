// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::local_index_cost_tests::{upload_body, BOUNDARY};
use crate::config::Config;
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, create_test_context_with_storage_and_config, send,
    send_with_headers, with_named_repositories, TestContext,
};
use axum::http::{Method, StatusCode};
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FILE: &str = "pkg-1.0.tar.gz";

fn local_config(config: &mut Config) {
    config.server.public_url = Some("https://packages.example/nora".into());
    config.server.trust_upstream_dates = false;
    config.repositories = toml::from_str::<Config>(
        r#"
[repositories.first]
format = "pypi"
kind = "local"
paths = ["/repos/python/", "/alias/python/", "/repos/a&b/"]
[repositories.second]
format = "pypi"
kind = "local"
paths = ["/second/"]
"#,
    )
    .unwrap()
    .repositories;
}

async fn publish(context: &TestContext, prefix: &str, bytes: &[u8]) {
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    let response = send_with_headers(
        &context.app,
        Method::POST,
        prefix,
        vec![("content-type", &content_type)],
        upload_body("pkg", "1.0", FILE, bytes),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn publications_and_aliases_are_isolated_and_keep_the_public_base_path() {
    let _lock = crate::test_env::env_lock();
    let context = with_named_repositories(create_test_context_with_config(local_config)).await;
    let ((), ()) = tokio::join!(
        publish(&context, "/repos/python/", b"first"),
        publish(&context, "/second/", b"second")
    );
    for (prefix, expected) in [
        ("/repos/python/", &b"first"[..]),
        ("/alias/python/", &b"first"[..]),
        ("/repos/a&b/", &b"first"[..]),
        ("/second/", &b"second"[..]),
    ] {
        let response = send_with_headers(
            &context.app,
            Method::GET,
            &format!("{prefix}pkg/"),
            vec![
                ("accept", "application/vnd.pypi.simple.v1+json"),
                ("host", "attacker.example"),
            ],
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        let url = format!("https://packages.example/nora{prefix}pkg/{FILE}");
        assert_eq!(json["files"][0]["url"], url);
        let response = send(
            &context.app,
            Method::GET,
            &format!("{prefix}pkg/{FILE}"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-security-policy"], "sandbox");
        assert_eq!(body_bytes(response).await.as_ref(), expected);
        for route in [prefix.to_string(), format!("{prefix}pkg/")] {
            let response = send(&context.app, Method::GET, &route, "").await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
            assert!(html.contains(&format!(
                "https://packages.example/nora{}pkg/",
                prefix.replace('&', "&amp;")
            )));
            assert!(!html.contains("attacker.example"));
        }
    }
    assert!(context
        .state
        .storage
        .list("pypi/")
        .await
        .unwrap()
        .is_empty());
    let duplicate = send_with_headers(
        &context.app,
        Method::POST,
        "/alias/python/",
        vec![(
            "content-type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )],
        upload_body("pkg", "1.0", FILE, b"replacement"),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn explicit_aliases_never_activate_unconfigured_singleton_routes() {
    let _lock = crate::test_env::env_lock();
    for prefix in ["/simple/", "/npm/"] {
        let context = with_named_repositories(create_test_context_with_config(|cfg| {
            local_config(cfg);
            let crate::config::repositories::RepositoryConfig::Local(local) =
                cfg.repositories.get_mut("first").unwrap()
            else {
                panic!()
            };
            local.paths = vec![prefix.into()];
        }))
        .await;
        context
            .state
            .storage
            .put("pypi/legacy/legacy-1.tar.gz", b"legacy")
            .await
            .unwrap();
        publish(&context, prefix, b"named").await;
        let response = send(
            &context.app,
            Method::GET,
            &format!("{prefix}pkg/{FILE}"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await.as_ref(), b"named");
        let response = send(&context.app, Method::GET, &format!("{prefix}legacy/"), "").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let context = with_named_repositories(create_test_context_with_config(local_config)).await;
    context
        .state
        .storage
        .put(&format!("pypi/pkg/{FILE}"), b"legacy")
        .await
        .unwrap();
    assert_eq!(
        send(&context.app, Method::POST, "/simple/", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            &format!("/simple/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        context
            .state
            .storage
            .get(&format!("pypi/pkg/{FILE}"))
            .await
            .unwrap()
            .as_ref(),
        b"legacy"
    );
    assert!(context
        .state
        .repositories
        .get("first")
        .unwrap()
        .storage
        .list("pypi/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn proxies_are_read_only_and_cache_only_inside_their_source_namespace() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let digest = hex::encode(sha2::Sha256::digest(b"proxied"));
    Mock::given(method("GET"))
        .and(path("/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<a href=\"/files/{FILE}#sha256={digest}\">{FILE}</a>"
        )))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"proxied".to_vec()))
        .mount(&upstream)
        .await;
    let storage =
        crate::storage::Storage::from_backend(Arc::new(crate::storage::ObjectStorage::in_memory()));
    let configure = |cfg: &mut Config| {
        local_config(cfg);
        let source = format!("[repositories.remote]\nformat='pypi'\nkind='proxy'\npaths=['/proxy/','/proxy-alias/']\nupstream='{}'", upstream.uri());
        cfg.repositories
            .extend(toml::from_str::<Config>(&source).unwrap().repositories);
    };
    let context = with_named_repositories(create_test_context_with_storage_and_config(
        storage.clone(),
        configure,
    ))
    .await;
    let rejected = send(&context.app, Method::POST, "/proxy/", "not multipart").await;
    assert_eq!(rejected.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(rejected.headers()["allow"], "GET, HEAD");
    publish(&context, "/repos/python/", b"published").await;
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let response = send(&context.app, Method::GET, &format!("/proxy/pkg/{FILE}"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"proxied");
    let member = context.state.repositories.get("remote").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = member.state(&context.state);
            if let Some(snapshot) = super::named::stored(&state, "pkg").await.unwrap() {
                if let Some(file) = snapshot
                    .project
                    .as_ref()
                    .and_then(|project| project.files.iter().find(|file| file.filename == FILE))
                {
                    if matches!(
                        super::cache::load(&state, "pkg", file, None).await,
                        Ok(Some(_))
                    ) {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let requests = upstream.received_requests().await.unwrap().len();
    let restarted = with_named_repositories(create_test_context_with_storage_and_config(
        storage,
        |cfg| {
            configure(cfg);
            let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
                cfg.repositories.get_mut("remote").unwrap()
            else {
                panic!()
            };
            proxy.paths = vec!["/new-proxy/".into()];
        },
    ))
    .await;
    let response = send(
        &restarted.app,
        Method::GET,
        &format!("/new-proxy/pkg/{FILE}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"proxied");
    assert_eq!(upstream.received_requests().await.unwrap().len(), requests);
    let response = send_with_headers(
        &restarted.app,
        Method::GET,
        "/new-proxy/pkg/",
        vec![("accept", "application/vnd.pypi.simple.v1+json")],
        "",
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(
        json["files"][0]["url"],
        format!("https://packages.example/nora/new-proxy/pkg/{FILE}")
    );
    assert_eq!(
        context
            .state
            .repositories
            .get("first")
            .unwrap()
            .storage
            .get(&format!("pypi/pkg/{FILE}"))
            .await
            .unwrap()
            .as_ref(),
        b"published"
    );
}

#[tokio::test]
async fn disabled_named_formats_do_not_mount_or_suppress_other_formats() {
    let _lock = crate::test_env::env_lock();
    let context = with_named_repositories(create_test_context_with_config(|cfg| {
        local_config(cfg);
        cfg.repositories.insert(
            "images".into(),
            toml::from_str("format='docker'\nkind='local'\n").unwrap(),
        );
        cfg.pypi.enabled = false;
        let crate::config::repositories::RepositoryConfig::Local(local) =
            cfg.repositories.get_mut("first").unwrap()
        else {
            panic!()
        };
        local.paths = vec!["/npm/".into()];
    }))
    .await;
    assert_eq!(
        send(&context.app, Method::GET, "/second/", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&context.app, Method::GET, "/npm/-/whoami", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&context.app, Method::GET, "/v2/", "").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn named_uploads_keep_existing_authentication_requirements() {
    let _lock = crate::test_env::env_lock();
    use base64::Engine;
    let context = with_named_repositories(
        crate::test_helpers::create_test_context_with_auth_and_config(
            &[("alice", "password")],
            |cfg| {
                local_config(cfg);
                cfg.auth.public_web_ui = true;
                let crate::config::repositories::RepositoryConfig::Local(local) =
                    cfg.repositories.get_mut("first").unwrap()
                else {
                    panic!()
                };
                local.paths.extend([
                    "/ui-packages/".into(),
                    "/api-docs-packages/".into(),
                    "/npm/".into(),
                ]);
            },
        ),
    )
    .await;
    for prefix in ["/repos/python/", "/ui-packages/", "/api-docs-packages/"] {
        for method in [Method::GET, Method::POST] {
            assert_eq!(
                send(&context.app, method, prefix, "").await.status(),
                StatusCode::UNAUTHORIZED,
                "{prefix}"
            );
        }
    }
    let measured = context
        .app
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            context.state.clone(),
            crate::metrics::metrics_middleware,
        ));
    let counter = crate::metrics::HTTP_REQUESTS_TOTAL.with_label_values(&["pypi", "TRACE", "401"]);
    let before = counter.get();
    for prefix in [
        "/repos/python/",
        "/ui-packages/",
        "/api-docs-packages/",
        "/npm/",
    ] {
        assert_eq!(
            send(&measured, Method::TRACE, prefix, "").await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(counter.get(), before + 4);
    let authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alice:password")
    );
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    let response = send_with_headers(
        &context.app,
        Method::POST,
        "/repos/python/",
        vec![
            ("authorization", &authorization),
            ("content-type", &content_type),
        ],
        upload_body("pkg", "1.0", FILE, b"authorized"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}
