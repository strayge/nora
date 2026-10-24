// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::Config;
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, create_test_context_with_storage_and_config, send,
    send_with_headers, with_named_repositories, TestContext,
};
use axum::http::Method;
use wiremock::matchers::{header as upstream_header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) const FILE: &str = "pkg-1.0-py3-none-any.whl";
const ACCEPT: &str = "application/vnd.pypi.simple.v1+json, text/html;q=0.1";

fn config(config: &mut Config, upstream: &str) {
    config.server.public_url = Some("https://nora.example/base".into());
    config.server.trust_upstream_dates = true;
    config.repositories = toml::from_str::<Config>(&format!(
        r#"
[repositories.remote]
format="pypi"
kind="proxy"
paths=["/python/", "/alias/"]
upstream="{upstream}/simple"
metadata_ttl=0
"#
    ))
    .unwrap()
    .repositories;
}

pub(super) async fn context(
    upstream: &str,
    storage: Option<crate::storage::Storage>,
    customize: impl FnOnce(&mut Config),
) -> TestContext {
    let configure = |cfg: &mut Config| {
        config(cfg, upstream);
        customize(cfg);
    };
    with_named_repositories(match storage {
        Some(storage) => create_test_context_with_storage_and_config(storage, configure),
        None => create_test_context_with_config(configure),
    })
    .await
}

pub(super) fn document(upstream: &str, status: &str) -> serde_json::Value {
    serde_json::json!({"meta":{"api-version":"1.4"}, "name":"pkg", "versions":["1.0", "2.0"],
        "project-status":{"status":status, "reason":"source reason"},
        "files":[{"filename":FILE,"url":format!("{upstream}/files/{FILE}"),
            "hashes":{"sha256":hex::encode(sha2::Sha256::digest(b"body")),"sha512":hex::encode(sha2::Sha512::digest(b"body"))},
            "requires-python":">=3.8,<4", "yanked":true, "core-metadata":{"sha256":"cd".repeat(32)},
            "size":4,"upload-time":"2020-01-02T03:04:05Z"}]})
}

pub(super) async fn mount(server: &MockServer, status: &str) {
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(document(&server.uri(), status)))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"body"))
        .mount(server)
        .await;
}

pub(super) async fn index(context: &TestContext, prefix: &str) -> (StatusCode, serde_json::Value) {
    let response = send_with_headers(
        &context.app,
        Method::GET,
        &format!("{prefix}pkg/"),
        vec![("accept", PEP691_JSON)],
        "",
    )
    .await;
    let status = response.status();
    let bytes = body_bytes(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

pub(super) async fn cached(context: &TestContext) {
    let repository = context.state.repositories.get("remote").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while repository
        .storage
        .stat(&format!("pypi/pkg/{FILE}"))
        .await
        .is_none()
        || repository
            .storage
            .pin(&format!("pypi/pkg/{FILE}"))
            .await
            .is_none()
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub(super) async fn settled(context: &TestContext) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !context.state.background_tasks.is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn json_first_metadata_is_rich_neutral_and_does_not_advertise_sidecars_early() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let context = context(&upstream.uri(), None, |_| {}).await;
    let (status, json) = index(&context, "/python/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"][0]["requires-python"], ">=3.8,<4");
    assert_eq!(json["files"][0]["yanked"], true);
    assert_eq!(json["versions"], serde_json::json!(["1.0", "2.0"]));
    assert_eq!(
        json["files"][0]["upload-time"],
        "2020-01-02T03:04:05.000000Z"
    );
    assert_eq!(
        json["files"][0]["url"],
        format!("https://nora.example/base/python/pkg/{FILE}")
    );
    assert!(json["files"][0].get("core-metadata").is_none());
    let snapshot = named::stored(
        &context
            .state
            .repositories
            .get("remote")
            .unwrap()
            .state(&context.state),
        "pkg",
    )
    .await
    .unwrap()
    .unwrap();
    assert!(snapshot.project.as_ref().unwrap().files[0]
        .core_metadata
        .as_ref()
        .unwrap()
        .available());
    assert!(snapshot.project.as_ref().unwrap().files[0]
        .url
        .starts_with(&upstream.uri()));
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("nora.example"));
    let response = send(&context.app, Method::GET, "/alias/pkg/", "").await;
    let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(html.contains("data-requires-python=\"&gt;=3.8,&lt;4\""));
    assert!(html.contains("data-yanked=\"\""));
    assert!(!html.contains("data-core-metadata"));
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.headers["accept"] == ACCEPT));
}

#[tokio::test]
async fn a_cold_file_uses_one_rich_index_instead_of_a_separate_dates_request() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let context = context(&upstream.uri(), None, |_| {}).await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"body");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/simple/pkg/")
            .count(),
        1
    );
    assert_eq!(requests.len(), 2);
    assert!(context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .storage
        .get("pypi/pkg/dates.json")
        .await
        .is_err());
}

#[tokio::test]
async fn malformed_or_authentication_metadata_never_triggers_html_fallback() {
    let _lock = crate::test_env::env_lock();
    for status in [200, 401, 403] {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/pkg/"))
            .and(header_regex(
                "accept",
                "^application/vnd[.]pypi[.]simple[.]v1[+]json",
            ))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("content-type", "application/vnd.pypi.simple.v1+json")
                    .set_body_string("{"),
            )
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(upstream_header("accept", "text/html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("<a href=\"{FILE}\">{FILE}</a>")),
            )
            .mount(&upstream)
            .await;
        let context = context(&upstream.uri(), None, |_| {}).await;
        assert_eq!(index(&context, "/python/").await.0, StatusCode::BAD_GATEWAY);
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/simple/pkg/");
    }
}

#[tokio::test]
async fn json_negotiation_and_html_fallback_share_the_metadata_deadline() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header_regex(
            "accept",
            "^application/vnd[.]pypi[.]simple[.]v1[+]json",
        ))
        .respond_with(ResponseTemplate::new(406).set_delay(Duration::from_millis(600)))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(upstream_header("accept", "text/html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("<a href=\"{FILE}\">{FILE}</a>"))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&upstream)
        .await;
    let context = context(&upstream.uri(), None, |cfg| {
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            cfg.repositories.get_mut("remote").unwrap()
        else {
            panic!()
        };
        proxy.metadata_timeout = 1;
    })
    .await;
    assert_eq!(index(&context, "/python/").await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn quarantine_survives_outage_restart_and_every_cached_file_method() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let context = context(&upstream.uri(), None, |_| {}).await;
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    cached(&context).await;
    upstream.reset().await;
    mount(&upstream, "quarantined").await;
    let (status, json) = index(&context, "/alias/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["files"].as_array().unwrap().is_empty());
    assert_eq!(json["project-status"]["status"], "quarantined");
    assert!(context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .pypi_snapshots
        .read()
        .is_empty());
    for method in [Method::GET, Method::HEAD] {
        assert_eq!(
            send_with_headers(
                &context.app,
                method,
                &format!("/python/pkg/{FILE}"),
                vec![("range", "bytes=1-2")],
                ""
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    let (_, json) = index(&context, "/python/").await;
    assert_eq!(json["project-status"]["status"], "quarantined");
    let restored =
        self::context(&upstream.uri(), Some(context.state.storage.clone()), |_| {}).await;
    assert_eq!(
        send(
            &restored.app,
            Method::GET,
            &format!("/alias/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    upstream.reset().await;
    mount(&upstream, "active").await;
    restored.state.circuit_breaker.expire_open_for_test(
        &restored
            .state
            .repositories
            .get("remote")
            .unwrap()
            .breaker_key(),
    );
    let _ = index(&restored, "/alias/").await;
    settled(&restored).await;
    assert_eq!(
        index(&restored, "/alias/").await.1["project-status"]["status"],
        "active"
    );
    assert_eq!(
        send(
            &restored.app,
            Method::GET,
            &format!("/alias/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn internal_namespaces_never_turn_proxy_caches_into_local_publications() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let original = context(&upstream.uri(), None, |_| {}).await;
    assert_eq!(
        send(
            &original.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    cached(&original).await;
    upstream.reset().await;
    let blocked = context(
        &upstream.uri(),
        Some(original.state.storage.clone()),
        |cfg| cfg.curation.internal_namespaces = vec!["pkg".into()],
    )
    .await;
    assert_eq!(index(&blocked, "/python/").await.0, StatusCode::FORBIDDEN);
    for method in [Method::GET, Method::HEAD] {
        assert_eq!(
            send_with_headers(
                &blocked.app,
                method,
                &format!("/alias/pkg/{FILE}"),
                vec![("range", "bytes=0-1")],
                ""
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let local = context(
        &upstream.uri(),
        Some(original.state.storage.clone()),
        |cfg| {
            cfg.curation.internal_namespaces = vec!["pkg".into()];
            cfg.repositories = toml::from_str::<Config>(
                "[repositories.hosted]\nformat='pypi'\nkind='local'\npaths=['/local/']\n",
            )
            .unwrap()
            .repositories;
        },
    )
    .await;
    let content_type = format!(
        "multipart/form-data; boundary={}",
        super::local_index_cost_tests::BOUNDARY
    );
    let response = send_with_headers(
        &local.app,
        Method::POST,
        "/local/",
        vec![("content-type", &content_type)],
        super::local_index_cost_tests::upload_body(
            "pkg",
            "1.0",
            "pkg-1.0.tar.gz",
            b"genuine-local",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send(&local.app, Method::GET, "/local/pkg/pkg-1.0.tar.gz", "").await;
    assert_eq!(body_bytes(response).await.as_ref(), b"genuine-local");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn first_seen_ages_survive_refresh_but_changed_expected_hashes_start_new_ages() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let context = context(&upstream.uri(), None, |cfg| {
        cfg.server.trust_upstream_dates = false;
        cfg.curation.mode = crate::config::CurationMode::Enforce;
        cfg.curation.min_release_age = Some("2d".into());
    })
    .await;
    assert_eq!(index(&context, "/python/").await.0, StatusCode::OK);
    let state = context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .state(&context.state);
    let key = ".nora-pypi-first-seen/pkg.json";
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&state.storage.get(key).await.unwrap()).unwrap();
    let old = crate::cache_ttl::now_unix() as i64 - 1000;
    ledger["files"][FILE]["first_seen"] = serde_json::json!(old);
    state
        .storage
        .put(key, &serde_json::to_vec(&ledger).unwrap())
        .await
        .unwrap();
    assert_eq!(index(&context, "/alias/").await.0, StatusCode::OK);
    assert_eq!(named::publish_date(&state, "pkg", FILE).await, Some(old));
    let mut changed = document(&upstream.uri(), "active");
    changed["files"][0]["hashes"]["sha256"] = serde_json::json!("ef".repeat(32));
    upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(changed))
        .mount(&upstream)
        .await;
    assert_eq!(index(&context, "/python/").await.0, StatusCode::OK);
    assert!(named::publish_date(&state, "pkg", FILE).await.unwrap() > old);
}

#[tokio::test]
async fn failed_snapshot_writes_cannot_replace_known_quarantine_with_active() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "quarantined").await;
    let (storage, controls) = crate::test_helpers::op_counting_storage();
    let context = context(&upstream.uri(), Some(storage), |_| {}).await;
    assert_eq!(
        index(&context, "/python/").await.1["project-status"]["status"],
        "quarantined"
    );
    let repository = context.state.repositories.get("remote").unwrap();
    controls.fail_writes_to(Some(&format!(
        "repositories/remote/sources/{}/{}",
        repository.source_id,
        named::SNAPSHOTS
    )));
    upstream.reset().await;
    mount(&upstream, "active").await;
    assert_eq!(
        index(&context, "/alias/").await.1["project-status"]["status"],
        "quarantined"
    );
    assert_eq!(
        send(&context.app, Method::GET, &format!("/alias/pkg/{FILE}"), "")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    controls.fail_writes_to(None);
    upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    let restored =
        self::context(&upstream.uri(), Some(context.state.storage.clone()), |_| {}).await;
    assert_eq!(
        send(
            &restored.app,
            Method::GET,
            &format!("/alias/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}
