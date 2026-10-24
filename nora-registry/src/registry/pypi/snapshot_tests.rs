// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::rich_tests::{cached, context, document, index, mount, settled, FILE};
use super::*;
use crate::config::{repositories::RepositoryConfig, Config};
use crate::test_helpers::{body_bytes, send, send_with_headers, TestContext};
use axum::http::Method;
use std::sync::Arc;
use wiremock::matchers::{header as upstream_header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ttl(config: &mut Config, value: u64) {
    let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
        panic!("proxy fixture")
    };
    policy.metadata_ttl = value;
    policy.metadata_timeout = 1;
}

async fn age(context: &TestContext, name: &str) {
    let repository = context.state.repositories.get("remote").unwrap();
    let view = repository.state(&context.state);
    let mut snapshot = named::stored(&view, name)
        .await
        .unwrap()
        .unwrap()
        .as_ref()
        .clone();
    snapshot.fetched_at = crate::cache_ttl::now_unix().saturating_sub(3600);
    repository
        .storage
        .put(
            &format!("{}{name}.json", named::SNAPSHOTS),
            &serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .unwrap();
}

async fn json(context: &TestContext, name: &str) -> (StatusCode, serde_json::Value) {
    let response = send_with_headers(
        &context.app,
        Method::GET,
        &format!("/python/{name}/"),
        vec![("accept", PEP691_JSON)],
        "",
    )
    .await;
    let status = response.status();
    let bytes = body_bytes(response).await;
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn fresh_snapshots_survive_restart_and_root_lists_metadata_only_projects_without_egress() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let original = context(&upstream.uri(), None, |config| ttl(config, 300)).await;
    assert_eq!(index(&original, "/python/").await.0, StatusCode::OK);
    assert!(original
        .state
        .repositories
        .get("remote")
        .unwrap()
        .storage
        .list("pypi/")
        .await
        .unwrap()
        .is_empty());
    upstream.reset().await;
    let (status, value) = index(&original, "/alias/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(value["files"][0]["url"]
        .as_str()
        .unwrap()
        .contains("/base/alias/pkg/"));
    let restored = context(
        &upstream.uri(),
        Some(original.state.storage.clone()),
        |config| ttl(config, 300),
    )
    .await;
    assert_eq!(index(&restored, "/python/").await.0, StatusCode::OK);
    let response = send_with_headers(
        &restored.app,
        Method::GET,
        "/alias/",
        vec![("accept", PEP691_JSON)],
        "",
    )
    .await;
    let root: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(root["projects"], serde_json::json!([{"name":"pkg"}]));
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn conditional_refresh_keeps_rich_attributes_and_updates_304_validators() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(document(&upstream.uri(), "archived"))
                .insert_header("etag", "\"v1\"")
                .insert_header("last-modified", "Wed, 01 Jan 2020 00:00:00 GMT"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(upstream_header("if-none-match", "\"v1\""))
        .and(header_regex(
            "if-modified-since",
            "01 Jan 2020 00:00:00 GMT",
        ))
        .respond_with(
            ResponseTemplate::new(304)
                .insert_header("etag", "W/\"v1\"")
                .insert_header("last-modified", "Thu, 02 Jan 2020 00:00:00 GMT"),
        )
        .mount(&upstream)
        .await;
    let context = context(&upstream.uri(), None, |config| ttl(config, 0)).await;
    let (_, initial) = index(&context, "/python/").await;
    age(&context, "pkg").await;
    let (status, refreshed) = index(&context, "/alias/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        refreshed["files"][0]["requires-python"],
        initial["files"][0]["requires-python"]
    );
    assert_eq!(
        refreshed["files"][0]["upload-time"],
        initial["files"][0]["upload-time"]
    );
    assert_eq!(
        refreshed["files"][0]["hashes"],
        initial["files"][0]["hashes"]
    );
    assert_eq!(refreshed["project-status"], initial["project-status"]);
    let repository = context.state.repositories.get("remote").unwrap();
    let stored = named::stored(&repository.state(&context.state), "pkg")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.validators.etag.as_deref(), Some("W/\"v1\""));
    assert_eq!(
        stored.validators.last_modified.as_deref(),
        Some("Thu, 02 Jan 2020 00:00:00 GMT")
    );
    assert!(crate::cache_ttl::now_unix().saturating_sub(stored.fetched_at) < 2);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn confirmed_absence_is_persistent_but_stale_absence_never_masks_an_error() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&upstream)
        .await;
    let original = context(&upstream.uri(), None, |config| ttl(config, 300)).await;
    assert_eq!(index(&original, "/python/").await.0, StatusCode::NOT_FOUND);
    assert_eq!(index(&original, "/alias/").await.0, StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.reset().await;
    let restored = context(
        &upstream.uri(),
        Some(original.state.storage.clone()),
        |config| ttl(config, 300),
    )
    .await;
    assert_eq!(index(&restored, "/alias/").await.0, StatusCode::NOT_FOUND);
    let response = send_with_headers(
        &restored.app,
        Method::GET,
        "/python/",
        vec![("accept", PEP691_JSON)],
        "",
    )
    .await;
    let root: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(root["projects"], serde_json::json!([]));
    assert!(upstream.received_requests().await.unwrap().is_empty());
    age(&restored, "pkg").await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    assert_eq!(
        index(&restored, "/python/").await.0,
        StatusCode::BAD_GATEWAY
    );
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_secs(2)))
        .mount(&upstream)
        .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(200), index(&restored, "/alias/"))
            .await
            .unwrap()
            .0,
        StatusCode::BAD_GATEWAY
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn offline_candidates_are_cached_only_with_original_constraints_and_no_network_storm() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let mut project = document(&upstream.uri(), "active");
    let mut uncached = project["files"][0].clone();
    uncached["filename"] = serde_json::json!("pkg-2.0-py3-none-any.whl");
    uncached["url"] =
        serde_json::json!(format!("{}/files/pkg-2.0-py3-none-any.whl", upstream.uri()));
    project["files"].as_array_mut().unwrap().push(uncached);
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(project))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"body"))
        .mount(&upstream)
        .await;
    let context = context(&upstream.uri(), None, |config| ttl(config, 0)).await;
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
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    let (status, value) = index(&context, "/python/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["files"].as_array().unwrap().len(), 1);
    assert_eq!(value["files"][0]["requires-python"], ">=3.8,<4");
    assert_eq!(value["files"][0]["yanked"], true);
    assert_eq!(value["versions"], serde_json::json!(["1.0"]));
    assert_eq!(value["files"][0]["size"], 4);
    for _ in 0..5 {
        assert_eq!(
            index(&context, "/alias/").await.1["versions"],
            serde_json::json!(["1.0"])
        );
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn shared_failures_and_half_open_success_survive_caller_cancellation() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let seen = Arc::new(tokio::sync::Notify::new());
    let notify = seen.clone();
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(move |_: &wiremock::Request| {
            notify.notify_one();
            ResponseTemplate::new(503).set_delay(Duration::from_millis(200))
        })
        .mount(&upstream)
        .await;
    let context = context(&upstream.uri(), None, |config| {
        ttl(config, 0);
        config.circuit_breaker.enabled = true;
        config.circuit_breaker.failure_threshold = 1;
    })
    .await;
    let app = context.app.clone();
    let caller = tokio::spawn(async move { send(&app, Method::GET, "/python/pkg/", "").await });
    tokio::time::timeout(Duration::from_secs(1), seen.notified())
        .await
        .unwrap();
    caller.abort();
    let mut followers = Vec::new();
    for _ in 0..8 {
        let app = context.app.clone();
        followers.push(tokio::spawn(async move {
            send(&app, Method::GET, "/alias/pkg/", "").await.status()
        }));
    }
    for follower in followers {
        assert_eq!(follower.await.unwrap(), StatusCode::BAD_GATEWAY);
    }
    settled(&context).await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let source = context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .breaker_key();
    assert_eq!(
        context
            .state
            .circuit_breaker
            .health_snapshot(&source)
            .unwrap()
            .status,
        "open"
    );
    upstream.reset().await;
    let notify = seen.clone();
    let project = document(&upstream.uri(), "active");
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(move |_: &wiremock::Request| {
            notify.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(&project)
                .set_delay(Duration::from_millis(200))
        })
        .mount(&upstream)
        .await;
    context.state.circuit_breaker.expire_open_for_test(&source);
    let app = context.app.clone();
    let caller = tokio::spawn(async move { send(&app, Method::GET, "/python/pkg/", "").await });
    tokio::time::timeout(Duration::from_secs(1), seen.notified())
        .await
        .unwrap();
    caller.abort();
    assert_eq!(index(&context, "/alias/").await.0, StatusCode::OK);
    settled(&context).await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let health = context
        .state
        .circuit_breaker
        .health_snapshot(&source)
        .unwrap();
    assert_eq!(health.status, "closed");
    assert_eq!(health.failure_count, 0);
}

#[tokio::test]
async fn disabled_enforcement_still_limits_background_recovery_to_one_source_probe() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let pkg = document(&upstream.uri(), "active");
    let mut other = pkg.clone();
    other["name"] = serde_json::json!("other");
    other["files"][0]["filename"] = serde_json::json!("other-1.0-py3-none-any.whl");
    other["files"][0]["url"] = serde_json::json!(format!(
        "{}/files/other-1.0-py3-none-any.whl",
        upstream.uri()
    ));
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&pkg))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/other/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&other))
        .mount(&upstream)
        .await;
    let context = context(&upstream.uri(), None, |config| ttl(config, 300)).await;
    assert_eq!(json(&context, "pkg").await.0, StatusCode::OK);
    assert_eq!(json(&context, "other").await.0, StatusCode::OK);
    let source = context
        .state
        .repositories
        .get("remote")
        .unwrap()
        .breaker_key();
    context
        .state
        .circuit_breaker
        .record_failure(&source, crate::circuit_breaker::ProbeToken::BACKGROUND);
    upstream.reset().await;
    assert!(json(&context, "pkg").await.1["files"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(upstream.received_requests().await.unwrap().is_empty());
    context.state.circuit_breaker.expire_open_for_test(&source);
    let seen = Arc::new(tokio::sync::Notify::new());
    let notify = seen.clone();
    Mock::given(method("GET"))
        .respond_with(move |request: &wiremock::Request| {
            notify.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(if request.url.path().contains("/other/") {
                    &other
                } else {
                    &pkg
                })
                .set_delay(Duration::from_millis(500))
        })
        .mount(&upstream)
        .await;
    assert!(json(&context, "pkg").await.1["files"]
        .as_array()
        .unwrap()
        .is_empty());
    tokio::time::timeout(Duration::from_secs(1), seen.notified())
        .await
        .unwrap();
    for name in ["other", "pkg", "other", "pkg"] {
        assert!(json(&context, name).await.1["files"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    settled(&context).await;
    assert!(!context.state.circuit_breaker.is_enabled());
    assert_eq!(
        context
            .state
            .circuit_breaker
            .health_snapshot(&source)
            .unwrap()
            .status,
        "closed"
    );
    assert_eq!(
        json(&context, "pkg").await.1["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn source_changes_never_adopt_a_previous_sources_snapshot() {
    let _lock = crate::test_env::env_lock();
    let first = MockServer::start().await;
    mount(&first, "active").await;
    let original = context(&first.uri(), None, |config| ttl(config, 300)).await;
    assert_eq!(index(&original, "/python/").await.0, StatusCode::OK);
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&second)
        .await;
    let changed = context(
        &second.uri(),
        Some(original.state.storage.clone()),
        |config| ttl(config, 300),
    )
    .await;
    assert_eq!(index(&changed, "/python/").await.0, StatusCode::BAD_GATEWAY);
    let old = original.state.repositories.get("remote").unwrap();
    let new = changed.state.repositories.get("remote").unwrap();
    assert_ne!(old.source_id, new.source_id);
    assert!(named::stored(&old.state(&original.state), "pkg")
        .await
        .unwrap()
        .is_some());
    assert!(named::stored(&new.state(&changed.state), "pkg")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn schema_one_snapshots_keep_fresh_reads_and_quarantine_until_a_successful_rewrite() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    for status in ["active", "quarantined"] {
        upstream.reset().await;
        let original = context(&upstream.uri(), None, |config| ttl(config, 300)).await;
        let repository = original.state.repositories.get("remote").unwrap();
        let page = reqwest::Url::parse(&format!("{}/simple/pkg/", upstream.uri())).unwrap();
        let project = model::parse_json(
            &serde_json::to_vec(&document(&upstream.uri(), status)).unwrap(),
            "pkg",
            &page,
        )
        .unwrap();
        let old = serde_json::json!({"schema":1,"source_id":repository.source_id,"fetched_at":crate::cache_ttl::now_unix(),
            "validators":{"etag":"\"v1\"","last_modified":null},"page_url":page.as_str(),"content_type":PEP691_JSON,"project":project});
        let key = format!("{}pkg.json", named::SNAPSHOTS);
        repository
            .storage
            .put(&key, &serde_json::to_vec(&old).unwrap())
            .await
            .unwrap();
        repository
            .storage
            .put(&format!("pypi/pkg/{FILE}"), b"body")
            .await
            .unwrap();
        assert_eq!(
            index(&original, "/alias/").await.1["project-status"]["status"],
            status
        );
        assert_eq!(
            send(
                &original.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                ""
            )
            .await
            .status(),
            if status == "active" {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        let restored = context(
            &upstream.uri(),
            Some(original.state.storage.clone()),
            |config| ttl(config, 300),
        )
        .await;
        assert_eq!(
            index(&restored, "/python/").await.1["project-status"]["status"],
            status
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &repository.storage.get(&key).await.unwrap()
            )
            .unwrap()["schema"],
            1
        );
        assert!(upstream.received_requests().await.unwrap().is_empty());
        Mock::given(method("GET"))
            .and(path("/simple/pkg/"))
            .and(upstream_header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&upstream)
            .await;
        let refresh = context(
            &upstream.uri(),
            Some(original.state.storage.clone()),
            |config| ttl(config, 0),
        )
        .await;
        assert_eq!(
            index(&refresh, "/alias/").await.1["project-status"]["status"],
            status
        );
        let rewritten: serde_json::Value =
            serde_json::from_slice(&repository.storage.get(&key).await.unwrap()).unwrap();
        assert_eq!(rewritten["schema"], 2);
        assert_eq!(rewritten["name"], "pkg");
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn concurrent_stale_reads_are_inside_the_shared_refresh_not_before_election() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let (storage, counts) = crate::test_helpers::op_counting_storage();
    let context = context(&upstream.uri(), Some(storage), |config| ttl(config, 0)).await;
    assert_eq!(index(&context, "/python/").await.0, StatusCode::OK);
    settled(&context).await;
    upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(document(&upstream.uri(), "active"))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&upstream)
        .await;
    counts.reset();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut callers = Vec::new();
    for _ in 0..8 {
        let app = context.app.clone();
        let barrier = barrier.clone();
        callers.push(tokio::spawn(async move {
            barrier.wait().await;
            send(&app, Method::GET, "/python/pkg/", "").await.status()
        }));
    }
    for caller in callers {
        assert_eq!(caller.await.unwrap(), StatusCode::OK);
    }
    settled(&context).await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        counts.snapshot().get("get"),
        Some(&2),
        "one initial read and one locked re-read, not one stale read per caller"
    );
}

#[tokio::test]
async fn refreshed_hashes_and_removed_membership_gate_full_and_range_cache_hits() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "active").await;
    let context = context(&upstream.uri(), None, |config| ttl(config, 0)).await;
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
    let bytes = b"new-body";
    let mut updated = document(&upstream.uri(), "active");
    updated["files"][0]["size"] = serde_json::json!(bytes.len());
    updated["files"][0]["hashes"] = serde_json::json!({"sha256":hex::encode(sha2::Sha256::digest(bytes)),"sha512":hex::encode(sha2::Sha512::digest(bytes))});
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&updated))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(&upstream)
        .await;
    let response = send_with_headers(
        &context.app,
        Method::GET,
        &format!("/alias/pkg/{FILE}"),
        vec![("range", "bytes=0-2")],
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), bytes);
    updated["files"] = serde_json::json!([]);
    upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated))
        .mount(&upstream)
        .await;
    for headers in [vec![], vec![("range", "bytes=0-2")]] {
        assert_eq!(
            send_with_headers(
                &context.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                headers,
                ""
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() == "/simple/pkg/"));
}

#[tokio::test]
async fn negative_snapshot_locations_never_persist_url_credentials_or_fragments() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let context = context(&upstream.uri(), None, |config| {
        ttl(config, 300);
        let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
            panic!("proxy fixture")
        };
        let mut url = reqwest::Url::parse(&policy.upstream).unwrap();
        url.set_username("cache-user").unwrap();
        url.set_password(Some("cache-password")).unwrap();
        url.set_fragment(Some("private-fragment"));
        policy.upstream = url.to_string();
    })
    .await;
    assert_eq!(index(&context, "/python/").await.0, StatusCode::NOT_FOUND);
    let repository = context.state.repositories.get("remote").unwrap();
    let bytes = repository
        .storage
        .get(&format!("{}pkg.json", named::SNAPSHOTS))
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    for value in ["cache-user", "cache-password", "private-fragment"] {
        assert!(!text.contains(value));
    }
    let mut malformed: serde_json::Value = serde_json::from_str(&text).unwrap();
    malformed.as_object_mut().unwrap().remove("project");
    repository
        .storage
        .put(
            &format!("{}pkg.json", named::SNAPSHOTS),
            &serde_json::to_vec(&malformed).unwrap(),
        )
        .await
        .unwrap();
    upstream.reset().await;
    assert_eq!(index(&context, "/alias/").await.0, StatusCode::BAD_GATEWAY);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn absent_and_invalid_refreshes_never_erase_known_quarantine() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    mount(&upstream, "quarantined").await;
    let original = context(&upstream.uri(), None, |config| ttl(config, 0)).await;
    assert_eq!(
        index(&original, "/python/").await.1["project-status"]["status"],
        "quarantined"
    );
    upstream.reset().await;
    assert_eq!(
        index(&original, "/alias/").await.1["project-status"]["status"],
        "quarantined"
    );
    let restored = context(
        &upstream.uri(),
        Some(original.state.storage.clone()),
        |config| ttl(config, 300),
    )
    .await;
    assert_eq!(
        send(
            &restored.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            ""
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&upstream)
        .await;
    let empty = context(&upstream.uri(), None, |config| ttl(config, 0)).await;
    assert_eq!(index(&empty, "/python/").await.0, StatusCode::BAD_GATEWAY);
}
