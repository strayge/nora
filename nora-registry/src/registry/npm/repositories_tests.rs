// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::Config;
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, create_test_context_with_storage_and_config, send,
    send_with_headers, with_named_repositories, TestContext,
};
use axum::http::Method;
use wiremock::matchers::{header as upstream_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NAME: &str = "@Scope/pkg";
const VERSION: &str = "1.0.0";

fn local_config(config: &mut Config) {
    config.server.public_url = Some("https://packages.example/nora".into());
    config.repositories = toml::from_str::<Config>(
        r#"
[repositories.first]
format = "npm"
kind = "local"
paths = ["/packages/", "/alias/"]
[repositories.second]
format = "npm"
kind = "local"
paths = ["/second/"]
"#,
    )
    .unwrap()
    .repositories;
}

fn proxy_config(config: &mut Config, upstream: &str, ttl: u64) {
    local_config(config);
    config.repositories = toml::from_str::<Config>(&format!(
        r#"
[repositories.remote]
format = "npm"
kind = "proxy"
paths = ["/packages/", "/alias/"]
upstream = "{upstream}"
metadata_ttl = {ttl}
auth = "alice:secret"
"#
    ))
    .unwrap()
    .repositories;
}

async fn publish(context: &TestContext, prefix: &str, bytes: &[u8]) {
    let document = serde_json::json!({
        "name": NAME,
        "versions": { VERSION: { "name": NAME, "version": VERSION, "dist": {} } },
        "dist-tags": { "latest": VERSION },
        "_attachments": { "pkg-1.0.0.tgz": { "data": base64::engine::general_purpose::STANDARD.encode(bytes) } }
    });
    assert_eq!(
        send(
            &context.app,
            Method::PUT,
            &format!("{prefix}{NAME}"),
            serde_json::to_vec(&document).unwrap()
        )
        .await
        .status(),
        StatusCode::CREATED
    );
}

async fn json(context: &TestContext, uri: &str) -> serde_json::Value {
    let response = send(&context.app, Method::GET, uri, "").await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

async fn cached(context: &TestContext, repository: &str, key: &str) {
    let repository = context.state.repositories.get(repository).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while repository.storage.get(&origin_key(key)).await.is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "cache provenance missing: {key}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn metadata(tarball: &str) -> serde_json::Value {
    serde_json::json!({
        "name": NAME,
        "dist-tags": { "latest": VERSION },
        "versions": { VERSION: { "name": NAME, "version": VERSION,
            "dist": { "tarball": tarball, "integrity": "sha512-preserved" } } }
    })
}

#[tokio::test]
async fn unsaved_tarballs_keep_packuments_and_security_records_without_artifact_companions() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata(&format!(
            "{}/file.tgz?token=private",
            upstream.uri()
        ))))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/file.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"unsaved"))
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300);
        config.server.trust_upstream_dates = false;
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.save_files = false;
        config.curation.npm.quarantine = Some(crate::digest_quarantine::QuarantineMode::Observe);
        config.curation.npm.quarantine_ttl = Some("1h".into());
    }))
    .await;
    let uri = format!("/alias/{NAME}/-/pkg-1.0.0.tgz");
    let response = send(&context.app, Method::GET, &uri, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"unsaved");
    assert!(context.state.background_tasks.is_empty());
    cached(&context, "remote", &upstream_packument_key(NAME)).await;
    let repository = context.state.repositories.get("remote").unwrap();
    let key = format!("npm/{NAME}/tarballs/pkg-1.0.0.tgz");
    for key in [&key, &format!("{key}.sha256"), &origin_key(&key)] {
        assert!(repository.storage.stat(key).await.is_none(), "{key}");
    }
    assert!(repository.storage.pin(&key).await.is_none());
    let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(b"unsaved")));
    assert!(!matches!(
        context.state.digest_store.check("npm", &digest, 3600),
        crate::digest_quarantine::QuarantineStatus::New
    ));
    assert_eq!(
        context
            .state
            .digest_store
            .record("npm", &digest, "not-an-origin", None)
            .upstream,
        repository.breaker_key()
    );
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    assert!(json(&context, &format!("/packages/{NAME}")).await["versions"][VERSION].is_object());
    assert_eq!(
        send(&context.app, Method::GET, &uri, "").await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(repository.storage.stat(&key).await.is_none());
}

#[tokio::test]
async fn mixed_cache_policies_and_flag_changes_preserve_trusted_files_and_local_publish() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(metadata(&format!("{}/file.tgz", upstream.uri()))),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/file.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"saved"))
        .mount(&upstream)
        .await;
    let configure = |config: &mut Config| {
        proxy_config(config, &upstream.uri(), 300);
        config.repositories.extend(
            toml::from_str::<Config>(&format!(
                r#"
[repositories.transient]
format="npm"
kind="proxy"
paths=["/transient/"]
upstream="{}"
save_files=false
[repositories.local]
format="npm"
kind="local"
paths=["/local/"]
"#,
                upstream.uri()
            ))
            .unwrap()
            .repositories,
        );
    };
    let context = with_named_repositories(create_test_context_with_config(configure)).await;
    for prefix in ["/packages/", "/transient/"] {
        assert_eq!(
            body_bytes(
                send(
                    &context.app,
                    Method::GET,
                    &format!("{prefix}{NAME}/-/pkg-1.0.0.tgz"),
                    ""
                )
                .await
            )
            .await
            .as_ref(),
            b"saved"
        );
    }
    let key = format!("npm/{NAME}/tarballs/pkg-1.0.0.tgz");
    cached(&context, "remote", &key).await;
    context.state.background_tasks.close();
    context.state.background_tasks.wait().await;
    let transient = context.state.repositories.get("transient").unwrap();
    for key in [&key, &format!("{key}.sha256"), &origin_key(&key)] {
        assert!(transient.storage.stat(key).await.is_none());
    }
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    let changed = with_named_repositories(create_test_context_with_storage_and_config(
        context.state.storage.clone(),
        |config| {
            configure(config);
            let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
                config.repositories.get_mut("remote").unwrap()
            else {
                unreachable!()
            };
            proxy.save_files = false;
        },
    ))
    .await;
    assert_eq!(
        body_bytes(
            send(
                &changed.app,
                Method::GET,
                &format!("/alias/{NAME}/-/pkg-1.0.0.tgz"),
                ""
            )
            .await
        )
        .await
        .as_ref(),
        b"saved"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    publish(&changed, "/local/", b"local").await;
    assert_eq!(
        body_bytes(
            send(
                &changed.app,
                Method::GET,
                &format!("/local/{NAME}/-/pkg-1.0.0.tgz"),
                ""
            )
            .await
        )
        .await
        .as_ref(),
        b"local"
    );
    assert!(changed
        .state
        .repositories
        .get("local")
        .unwrap()
        .storage
        .stat(&key)
        .await
        .is_some());
}

/// The caller can observe queued jobs before a current-thread executor polls their writers.
#[tokio::test(flavor = "current_thread")]
async fn named_tarball_cache_jobs_are_tracked_and_force_cancellation_skips_their_writes() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(metadata(&format!("{}/file.tgz", upstream.uri()))),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/file.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tracked"))
        .mount(&upstream)
        .await;
    for cancelled in [false, true] {
        let context = with_named_repositories(create_test_context_with_config(|config| {
            proxy_config(config, &upstream.uri(), 300)
        }))
        .await;
        if cancelled {
            context.state.background_cancel.cancel();
        }
        let response = send(
            &context.app,
            Method::GET,
            &format!("/alias/{NAME}/-/pkg-1.0.0.tgz"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!context.state.background_tasks.is_empty());
        assert_eq!(body_bytes(response).await.as_ref(), b"tracked");
        context.state.background_tasks.close();
        tokio::time::timeout(
            Duration::from_secs(3),
            context.state.background_tasks.wait(),
        )
        .await
        .unwrap();
        let repository = context.state.repositories.get("remote").unwrap();
        let key = format!("npm/{NAME}/tarballs/pkg-1.0.0.tgz");
        for key in [&key, &format!("{key}.sha256"), &origin_key(&key)] {
            assert_eq!(
                repository.storage.stat(key).await.is_some(),
                !cancelled,
                "{key}"
            );
        }
    }
}

#[tokio::test]
async fn local_aliases_are_request_local_and_publication_records_are_neutral() {
    let _lock = crate::test_env::env_lock();
    let context = with_named_repositories(create_test_context_with_config(local_config)).await;
    tokio::join!(
        publish(&context, "/packages/", b"first"),
        publish(&context, "/second/", b"second")
    );
    for _ in 0..3 {
        let (first, alias) = tokio::join!(
            json(&context, "/packages/@Scope/pkg"),
            json(&context, "/alias/@Scope/pkg")
        );
        assert_eq!(
            first["versions"][VERSION]["dist"]["tarball"],
            "https://packages.example/nora/packages/@Scope/pkg/-/pkg-1.0.0.tgz"
        );
        assert_eq!(
            alias["versions"][VERSION]["dist"]["tarball"],
            "https://packages.example/nora/alias/@Scope/pkg/-/pkg-1.0.0.tgz"
        );
    }
    for prefix in ["/packages/", "/alias/", "/second/"] {
        for spec in [VERSION, "latest"] {
            let document = json(&context, &format!("{prefix}{NAME}/{spec}")).await;
            assert_eq!(
                document["dist"]["tarball"],
                format!("https://packages.example/nora{prefix}{NAME}/-/pkg-1.0.0.tgz")
            );
        }
        let response = send_with_headers(
            &context.app,
            Method::GET,
            &format!("{prefix}{NAME}"),
            vec![("accept", NPM_INSTALL_V1), ("host", "attacker.example")],
            "",
        )
        .await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], NPM_INSTALL_V1);
        assert!(!String::from_utf8(body_bytes(response).await.to_vec())
            .unwrap()
            .contains("attacker.example"));
        let response = send(
            &context.app,
            Method::GET,
            &format!("{prefix}{NAME}/-/pkg-1.0.0.tgz"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-security-policy"], "sandbox");
        assert_eq!(
            body_bytes(response).await.as_ref(),
            if prefix == "/second/" {
                &b"second"[..]
            } else {
                &b"first"[..]
            }
        );
    }
    let member = context.state.repositories.get("first").unwrap();
    for key in [
        published_packument_key(NAME),
        format!("npm/{NAME}/versions/{VERSION}.json"),
    ] {
        let bytes = member.storage.get(&key).await.unwrap();
        assert!(!String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("https://"));
    }
    assert!(context.state.repository_mount.is_none());
    assert!(member.state(&context.state).repository_mount.is_none());
    assert!(context.state.storage.list("npm/").await.unwrap().is_empty());
}

#[tokio::test]
async fn buffered_artifacts_keep_decoding_encoded_tarball_representations() {
    use std::io::Write;
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let bytes = b"tarball representation";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    let compressed = encoder.finish().unwrap();
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(bytes))
    );
    let mut document = metadata(&format!("{}/file.tgz", upstream.uri()));
    document["versions"][VERSION]["dist"]["integrity"] = serde_json::json!(integrity);
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&document))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/file.tgz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(compressed)
                .insert_header("content-encoding", "gzip"),
        )
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300)
    }))
    .await;
    let uri = format!("/packages/{NAME}/-/pkg-1.0.0.tgz");
    let response = send(&context.app, Method::GET, &uri, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let fetched = body_bytes(response).await;
    assert_eq!(fetched.as_ref(), bytes);
    assert_eq!(
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&fetched))
        ),
        integrity
    );
    cached(
        &context,
        "remote",
        &format!("npm/{NAME}/tarballs/pkg-1.0.0.tgz"),
    )
    .await;
    upstream.reset().await;
    assert_eq!(
        body_bytes(send(&context.app, Method::GET, &uri, "").await)
            .await
            .as_ref(),
        bytes
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn proxy_cdn_files_are_bound_to_the_exact_version_without_cross_origin_credentials() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let cdn = MockServer::start().await;
    let document = metadata(&format!("{}/objects/not-the-package-filename", cdn.uri()));
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .and(upstream_header("authorization", "Basic YWxpY2U6c2VjcmV0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&document))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/objects/not-the-package-filename"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"cdn-bytes"))
        .mount(&cdn)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300)
    }))
    .await;
    let file = format!("npm/{NAME}/tarballs/pkg-1.0.0.tgz");
    let response = send(
        &context.app,
        Method::GET,
        &format!("/alias/{NAME}/-/pkg-1.0.0.tgz"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"cdn-bytes");
    cached(&context, "remote", &file).await;
    let requests = cdn.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
    let stored = context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .storage
        .get(&upstream_packument_key(NAME))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stored).unwrap(),
        document
    );
    let rendered = json(&context, &format!("/packages/{NAME}")).await;
    assert_eq!(
        rendered["versions"][VERSION]["dist"]["integrity"],
        "sha512-preserved"
    );
    assert_eq!(
        rendered["versions"][VERSION]["dist"]["tarball"],
        "https://packages.example/nora/packages/@Scope/pkg/-/pkg-1.0.0.tgz"
    );
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            "/packages/@Scope/pkg/-/arbitrary-1.0.0.tgz",
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            "/packages/@Scope/pkg/-/pkg-9.0.0.tgz",
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &context.app,
            Method::PUT,
            "/packages/@Scope/pkg",
            "not json"
        )
        .await
        .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    upstream.reset().await;
    cdn.reset().await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/packages/{NAME}/-/pkg-1.0.0.tgz"),
        "",
    )
    .await;
    assert_eq!(body_bytes(response).await.as_ref(), b"cdn-bytes");
    let restored = with_named_repositories(create_test_context_with_storage_and_config(
        context.state.storage.clone(),
        |config| proxy_config(config, &upstream.uri(), 300),
    ))
    .await;
    let document = json(&restored, &format!("/alias/{NAME}")).await;
    assert_eq!(
        document["versions"][VERSION]["dist"]["tarball"],
        "https://packages.example/nora/alias/@Scope/pkg/-/pkg-1.0.0.tgz"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(cdn.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn conditional_refresh_and_offline_metadata_keep_raw_storage_and_current_alias() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let document = metadata(&format!("{}/original/pkg.tgz", upstream.uri()));
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"raw-v1\"")
                .set_body_json(&document),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}")))
        .and(upstream_header("if-none-match", "\"raw-v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 0)
    }))
    .await;
    json(&context, &format!("/packages/{NAME}")).await;
    cached(&context, "remote", &upstream_packument_key(NAME)).await;
    for prefix in ["/alias/", "/packages/"] {
        let document = json(&context, &format!("{prefix}{NAME}")).await;
        assert_eq!(
            document["versions"][VERSION]["dist"]["tarball"],
            format!("https://packages.example/nora{prefix}{NAME}/-/pkg-1.0.0.tgz")
        );
        let member = context.state.repositories.get("remote").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while read_validators(&member.storage, &upstream_packument_key(NAME))
            .await
            .is_none()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "validators not persisted"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.headers.contains_key("if-none-match")));
    upstream.reset().await;
    let response = send(&context.app, Method::GET, &format!("/alias/{NAME}"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-nora-stale"], "true");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body_bytes(response).await).unwrap()
            ["versions"][VERSION]["dist"]["tarball"],
        "https://packages.example/nora/alias/@Scope/pkg/-/pkg-1.0.0.tgz"
    );
    let stored = context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .storage
        .get(&upstream_packument_key(NAME))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stored).unwrap(),
        document
    );
}

#[tokio::test]
async fn unscoped_proxy_cache_requires_provenance_and_source_changes_start_empty() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plain"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"name":"plain", "versions":{}})),
        )
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300)
    }))
    .await;
    let member = context.state.repositories.get("remote").unwrap();
    let key = upstream_packument_key("plain");
    member
        .storage
        .put(&key, br#"{"name":"tainted","versions":{}}"#)
        .await
        .unwrap();
    assert_eq!(json(&context, "/packages/plain").await["name"], "plain");
    cached(&context, "remote", &key).await;
    member
        .storage
        .put(&key, br#"{"name":"tampered","versions":{}}"#)
        .await
        .unwrap();
    assert_eq!(json(&context, "/alias/plain").await["name"], "plain");
    cached(&context, "remote", &key).await;
    let replacement = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&replacement)
        .await;
    let changed = with_named_repositories(create_test_context_with_storage_and_config(
        context.state.storage.clone(),
        |config| proxy_config(config, &replacement.uri(), 300),
    ))
    .await;
    assert_eq!(
        send(&changed.app, Method::GET, "/packages/plain", "")
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(changed
        .state
        .repositories
        .get("remote")
        .unwrap()
        .storage
        .get(&key)
        .await
        .is_err());
}

#[tokio::test]
async fn named_audit_is_read_access_and_never_inherits_write_permission() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/-/npm/v1/security/advisories/bulk"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"pkg": ["real-advisory"]})),
        )
        .mount(&upstream)
        .await;
    let context = with_named_repositories(
        crate::test_helpers::create_test_context_with_auth_and_config(
            &[("alice", "password")],
            |config| {
                proxy_config(config, &upstream.uri(), 300);
                config.auth.anonymous_read = true;
            },
        ),
    )
    .await;
    let response = send(
        &context.app,
        Method::POST,
        "/alias/-/npm/v1/security/advisories/bulk",
        r#"{"pkg":["1.0.0"]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body_bytes(response).await).unwrap()["pkg"][0],
        "real-advisory"
    );
    assert_eq!(
        send(&context.app, Method::PUT, "/alias/pkg", "{}")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&context.app, Method::GET, "/alias/-/whoami", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let measured = context
        .app
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            context.state.clone(),
            crate::metrics::metrics_middleware,
        ));
    let counter = crate::metrics::HTTP_REQUESTS_TOTAL.with_label_values(&["npm", "TRACE", "401"]);
    let before = counter.get();
    assert_eq!(
        send(&measured, Method::TRACE, "/alias/pkg", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(counter.get(), before + 1);
}

#[tokio::test]
async fn malformed_version_metadata_never_advertises_a_direct_cdn_tarball() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{NAME}/{VERSION}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": NAME, "dist": {"tarball": "https://cdn.example/bypass.tgz"}
        })))
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300)
    }))
    .await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/alias/{NAME}/{VERSION}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!String::from_utf8(body_bytes(response).await.to_vec())
        .unwrap()
        .contains("cdn.example"));
    let response = send(&context.app, Method::POST, "/packages/unsupported", "").await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
}

#[tokio::test]
async fn authenticated_named_requests_follow_only_explicit_origin_scoped_redirects() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    for (verb, from, target, status) in [
        ("GET", format!("/{NAME}"), "/follow-metadata", 302),
        ("GET", "/redirect.tgz".into(), "/follow-file", 302),
        (
            "POST",
            "/-/npm/v1/security/advisories/bulk".into(),
            "/follow-audit",
            307,
        ),
    ] {
        Mock::given(method(verb))
            .and(path(from))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", format!("{}{target}", upstream.uri())),
            )
            .mount(&upstream)
            .await;
    }
    Mock::given(path("/follow-metadata"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(metadata(&format!("{}/redirect.tgz", upstream.uri()))),
        )
        .mount(&upstream)
        .await;
    Mock::given(path("/follow-file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"followed"))
        .mount(&upstream)
        .await;
    Mock::given(path("/follow-audit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"real":true})))
        .mount(&upstream)
        .await;
    let context = with_named_repositories(create_test_context_with_config(|config| {
        proxy_config(config, &upstream.uri(), 300)
    }))
    .await;
    assert_eq!(
        send(&context.app, Method::GET, &format!("/packages/{NAME}"), "")
            .await
            .status(),
        StatusCode::OK
    );
    let response = send(
        &context.app,
        Method::GET,
        &format!("/packages/{NAME}/-/pkg-1.0.0.tgz"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"followed");
    assert_eq!(
        send(
            &context.app,
            Method::POST,
            "/alias/-/npm/v1/security/advisories/bulk",
            "{}"
        )
        .await
        .status(),
        StatusCode::OK
    );
    let requests = upstream.received_requests().await.unwrap();
    let followed: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path().starts_with("/follow-"))
        .collect();
    assert!(followed.len() >= 3);
    assert!(followed
        .iter()
        .all(|request| request.headers["authorization"] == "Basic YWxpY2U6c2VjcmV0"));
}
