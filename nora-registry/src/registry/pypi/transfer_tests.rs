// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::rich_tests::{cached, context, document, index, settled, FILE};
use super::*;
use crate::config::{repositories::RepositoryConfig, Config};
use crate::test_helpers::{body_bytes, send, TestContext};
use axum::body::{Body, Bytes};
use axum::http::Method;
use axum::routing::get;
use futures::{StreamExt, TryStreamExt};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Source {
    uri: String,
    calls: Arc<AtomicUsize>,
    sent: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn source(chunks: usize, delay: Duration, bad_hash: bool, truncated: bool) -> Source {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
    let mut hash = sha2::Sha256::new();
    for _ in 0..chunks {
        hash.update(&chunk);
    }
    let digest = if bad_hash {
        "00".repeat(32)
    } else {
        hex::encode(hash.finalize())
    };
    let mut metadata = document(&uri, "active");
    metadata["files"][0]["hashes"] = serde_json::json!({"sha256":digest});
    metadata["files"][0]["size"] = serde_json::json!(chunks * chunk.len());
    metadata["files"][0]["yanked"] = serde_json::json!(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let sent = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(AtomicU16::new(200));
    let metadata_status = status.clone();
    let count = calls.clone();
    let sent_chunks = sent.clone();
    let app = Router::new()
        .route(
            "/simple/pkg/",
            get(move || {
                let metadata = metadata.clone();
                let status = metadata_status.clone();
                async move {
                    (
                        StatusCode::from_u16(status.load(Ordering::Relaxed)).unwrap(),
                        axum::Json(metadata),
                    )
                }
            }),
        )
        .route(
            &format!("/files/{FILE}"),
            get(move || {
                let chunk = chunk.clone();
                let sent = sent_chunks.clone();
                count.fetch_add(1, Ordering::Relaxed);
                async move {
                    let size = chunks * chunk.len();
                    let actual = chunks.saturating_sub(usize::from(truncated));
                    let stream = futures::stream::unfold(
                        (0, chunk, sent),
                        move |(index, chunk, sent)| async move {
                            if index == actual {
                                return None;
                            }
                            tokio::time::sleep(delay).await;
                            sent.fetch_add(1, Ordering::Relaxed);
                            Some((
                                Ok::<_, std::io::Error>(chunk.clone()),
                                (index + 1, chunk, sent),
                            ))
                        },
                    );
                    let mut response = Body::from_stream(stream).into_response();
                    response
                        .headers_mut()
                        .insert(header::CONTENT_LENGTH, size.to_string().parse().unwrap());
                    response
                }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Source {
        uri,
        calls,
        sent,
        status,
        task,
    }
}

fn policy(config: &mut Config) {
    let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
        panic!("proxy fixture")
    };
    policy.metadata_ttl = 300;
    policy.metadata_timeout = 1;
    policy.read_timeout = 1;
    policy.total_timeout = 0;
    config.pypi.proxy_timeout = 1;
}

async fn consume(response: Response) -> (u64, usize, String) {
    let mut stream = response.into_body().into_data_stream();
    let mut size = 0;
    let mut maximum = 0;
    let mut hash = sha2::Sha256::new();
    while let Some(chunk) = stream.try_next().await.unwrap() {
        size += chunk.len() as u64;
        maximum = maximum.max(chunk.len());
        hash.update(&chunk);
    }
    (size, maximum, hex::encode(hash.finalize()))
}

async fn not_ready(context: &TestContext) {
    settled(context).await;
    let repository = context.state.repositories.get("remote").unwrap();
    assert!(repository
        .storage
        .stat(&super::cache::ready_key("pkg", FILE))
        .await
        .is_none());
}

#[tokio::test]
async fn cold_stream_starts_early_warm_stream_uses_readers_and_readiness_survives_restart() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(32, Duration::from_millis(15), false, false).await;
    let (storage, counts) = crate::test_helpers::op_counting_storage();
    let original = context(&upstream.uri, Some(storage), policy).await;
    let response = send(
        &original.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    assert!(upstream.sent.load(Ordering::Relaxed) < 32);
    let repository = original.state.repositories.get("remote").unwrap();
    assert!(repository
        .storage
        .stat(&super::cache::ready_key("pkg", FILE))
        .await
        .is_none());
    let mut bytes = first.len() as u64;
    let mut maximum = first.len();
    while let Some(chunk) = stream.try_next().await.unwrap() {
        bytes += chunk.len() as u64;
        maximum = maximum.max(chunk.len());
    }
    assert_eq!(bytes, 32 * 64 * 1024);
    assert!(maximum <= 256 * 1024);
    cached(&original).await;
    counts.reset();
    let warm = consume(
        send(
            &original.app,
            Method::GET,
            &format!("/alias/pkg/{FILE}"),
            "",
        )
        .await,
    )
    .await;
    assert_eq!(warm.0, bytes);
    assert!(warm.1 <= 256 * 1024);
    assert_eq!(counts.snapshot().get("get_reader"), Some(&1));
    assert!(
        counts.snapshot().get("get").copied().unwrap_or(0) <= 3,
        "only small source and readiness records may use buffered get"
    );
    assert_eq!(upstream.calls.load(Ordering::Relaxed), 1);
    let restored = context(&upstream.uri, Some(original.state.storage.clone()), policy).await;
    upstream.status.store(503, Ordering::Relaxed);
    assert_eq!(
        consume(
            send(
                &restored.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                ""
            )
            .await
        )
        .await
        .0,
        bytes
    );
    assert_eq!(upstream.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn shared_fill_outlives_a_disconnected_client_and_finishes_a_valid_cache_entry() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(16, Duration::from_millis(20), false, false).await;
    let context = context(&upstream.uri, None, policy).await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);
    let follower = send(&context.app, Method::GET, &format!("/alias/pkg/{FILE}"), "").await;
    assert_eq!(consume(follower).await.0, 16 * 64 * 1024);
    cached(&context).await;
    assert_eq!(upstream.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn progressing_transfer_outlives_the_old_timeout_but_total_budget_still_applies() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(16, Duration::from_millis(80), false, false).await;
    let uncapped = context(&upstream.uri, None, policy).await;
    assert_eq!(
        consume(
            send(
                &uncapped.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                ""
            )
            .await
        )
        .await
        .0,
        16 * 64 * 1024
    );
    cached(&uncapped).await;
    let capped = context(&upstream.uri, None, |config| {
        policy(config);
        let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
            panic!("proxy fixture")
        };
        policy.total_timeout = 1;
    })
    .await;
    let response = send(&capped.app, Method::GET, &format!("/python/pkg/{FILE}"), "").await;
    if response.status() == StatusCode::OK {
        assert!(axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .is_err());
    } else {
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
    not_ready(&capped).await;
}

#[tokio::test]
async fn digest_mismatch_and_truncation_cannot_complete_or_become_offline_candidates() {
    let _lock = crate::test_env::env_lock();
    for (bad_hash, truncated) in [(true, false), (false, true)] {
        let upstream = source(8, Duration::from_millis(10), bad_hash, truncated).await;
        let context = context(&upstream.uri, None, policy).await;
        let response = send(
            &context.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            "",
        )
        .await;
        if response.status() == StatusCode::OK {
            assert!(axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .is_err());
        } else {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        }
        not_ready(&context).await;
        let (_, offline) = index(&context, "/python/").await;
        assert_eq!(offline["files"], serde_json::json!([]));
    }
}

#[tokio::test]
async fn quarantine_enforcement_waits_for_the_whole_body_before_releasing_any_artifact_bytes() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(8, Duration::from_millis(10), false, false).await;
    let context = context(&upstream.uri, None, |config| {
        policy(config);
        config.server.trust_upstream_dates = false;
        config.curation.pypi.quarantine = Some(crate::digest_quarantine::QuarantineMode::Enforce);
        config.curation.pypi.quarantine_ttl = Some("2d".into());
    })
    .await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(upstream.sent.load(Ordering::Relaxed), 8);
    assert!(response.headers().contains_key("x-nora-quarantine"));
    let body = body_bytes(response).await;
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    not_ready(&context).await;
}

#[tokio::test]
async fn body_and_companion_write_failures_never_publish_readiness() {
    let _lock = crate::test_env::env_lock();
    for companion in [false, true] {
        let upstream = source(4, Duration::from_millis(5), false, false).await;
        let (storage, controls) = crate::test_helpers::op_counting_storage();
        let context = context(&upstream.uri, Some(storage), |config| {
            policy(config);
            let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap()
            else {
                panic!("proxy fixture")
            };
            policy.metadata_ttl = 0;
        })
        .await;
        let repository = context.state.repositories.get("remote").unwrap();
        controls.fail_writes_to(Some(&format!(
            "repositories/remote/sources/{}/{}",
            repository.source_id,
            if companion {
                super::cache::READY
            } else {
                "pypi/"
            }
        )));
        assert_eq!(
            consume(
                send(
                    &context.app,
                    Method::GET,
                    &format!("/python/pkg/{FILE}"),
                    ""
                )
                .await
            )
            .await
            .0,
            4 * 64 * 1024
        );
        not_ready(&context).await;
        assert_eq!(
            repository
                .storage
                .stat(&super::cache::body_key("pkg", FILE))
                .await
                .is_some(),
            companion
        );
        upstream.status.store(503, Ordering::Relaxed);
        assert_eq!(
            index(&context, "/python/").await.1["files"],
            serde_json::json!([])
        );
    }
}

#[tokio::test]
async fn replacing_a_body_cannot_leave_the_previous_ready_marker_eligible_after_companion_failure()
{
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    super::rich_tests::mount(&upstream, "active").await;
    let (storage, controls) = crate::test_helpers::op_counting_storage();
    let context = context(&upstream.uri(), Some(storage), |config| {
        policy(config);
        let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
            panic!("proxy fixture")
        };
        policy.metadata_ttl = 0;
    })
    .await;
    assert_eq!(
        body_bytes(
            send(
                &context.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                ""
            )
            .await
        )
        .await
        .as_ref(),
        b"body"
    );
    cached(&context).await;
    let repository = context.state.repositories.get("remote").unwrap();
    controls.fail_writes_to(Some(&format!(
        "repositories/remote/sources/{}/{}",
        repository.source_id,
        super::cache::READY
    )));
    upstream.reset().await;
    let bytes = b"new-body";
    let mut metadata = document(&upstream.uri(), "active");
    metadata["files"][0]["hashes"] =
        serde_json::json!({"sha256":hex::encode(sha2::Sha256::digest(bytes))});
    metadata["files"][0]["size"] = serde_json::json!(bytes.len());
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(&upstream)
        .await;
    assert_eq!(
        body_bytes(send(&context.app, Method::GET, &format!("/alias/pkg/{FILE}"), "").await)
            .await
            .as_ref(),
        bytes
    );
    not_ready(&context).await;
    assert_eq!(
        repository
            .storage
            .pin(&super::cache::body_key("pkg", FILE))
            .await
            .unwrap(),
        hex::encode(sha2::Sha256::digest(bytes))
    );
    upstream.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;
    assert_eq!(
        index(&context, "/python/").await.1["files"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn forced_task_shutdown_aborts_an_incomplete_stream_and_cleans_owned_temps() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(100, Duration::from_millis(30), false, false).await;
    let context = context(&upstream.uri, None, policy).await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    context.state.background_tasks.close();
    context.state.background_cancel.cancel();
    tokio::time::timeout(
        Duration::from_secs(1),
        context.state.background_tasks.wait(),
    )
    .await
    .unwrap();
    assert!(body.try_collect::<Vec<_>>().await.is_err());
    not_ready(&context).await;
    let mut entries = tokio::fs::read_dir(super::transfer::temp_dir(
        &context.state.config.storage.path,
    ))
    .await
    .unwrap();
    assert!(entries.next_entry().await.unwrap().is_none());
}

#[tokio::test]
async fn earlier_named_cache_bytes_gain_readiness_without_egress_after_full_source_verification() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(4, Duration::from_millis(5), false, false).await;
    let original = context(&upstream.uri, None, |config| {
        policy(config);
        let RepositoryConfig::Proxy(policy) = config.repositories.get_mut("remote").unwrap() else {
            panic!("proxy fixture")
        };
        policy.metadata_ttl = 0;
    })
    .await;
    assert_eq!(index(&original, "/python/").await.0, StatusCode::OK);
    let repository = original.state.repositories.get("remote").unwrap();
    let bytes = vec![b'x'; 4 * 64 * 1024];
    let digest = hex::encode(sha2::Sha256::digest(&bytes));
    let key = super::cache::body_key("pkg", FILE);
    repository.storage.put(&key, &bytes).await.unwrap();
    repository
        .storage
        .put(&format!("{key}.sha256"), digest.as_bytes())
        .await
        .unwrap();
    put_hash_marker(&repository.storage, "pkg", FILE, &digest).await;
    assert!(repository
        .storage
        .stat(&super::cache::ready_key("pkg", FILE))
        .await
        .is_none());
    upstream.status.store(503, Ordering::Relaxed);
    let (_, offline) = index(&original, "/alias/").await;
    assert_eq!(offline["versions"], serde_json::json!(["1.0"]));
    assert_eq!(offline["files"][0]["requires-python"], ">=3.8,<4");
    assert!(repository
        .storage
        .stat(&super::cache::ready_key("pkg", FILE))
        .await
        .is_some());
    let restored = context(&upstream.uri, Some(original.state.storage.clone()), policy).await;
    assert_eq!(
        consume(
            send(
                &restored.app,
                Method::GET,
                &format!("/python/pkg/{FILE}"),
                ""
            )
            .await
        )
        .await
        .0,
        bytes.len() as u64
    );
    assert_eq!(upstream.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn readiness_migration_requires_strong_binding_all_supported_hashes_and_non_quarantine() {
    let _lock = crate::test_env::env_lock();
    for case in ["weak", "inconsistent", "quarantined"] {
        let upstream = source(2, Duration::from_millis(5), false, false).await;
        let context = context(&upstream.uri, None, policy).await;
        assert_eq!(index(&context, "/python/").await.0, StatusCode::OK);
        let repository = context.state.repositories.get("remote").unwrap();
        repository
            .storage
            .put(
                &super::cache::body_key("pkg", FILE),
                &vec![b'x'; 2 * 64 * 1024],
            )
            .await
            .unwrap();
        let mut snapshot = named::stored(&repository.state(&context.state), "pkg")
            .await
            .unwrap()
            .unwrap()
            .as_ref()
            .clone();
        let project = Arc::make_mut(snapshot.project.as_mut().unwrap());
        match case {
            "weak" => project.files[0].hashes.clear(),
            "inconsistent" => {
                project.files[0]
                    .hashes
                    .insert("sha512".into(), "00".repeat(64));
            }
            _ => project.status.status = model::Status::Quarantined,
        }
        repository
            .storage
            .put(
                &format!("{}pkg.json", named::SNAPSHOTS),
                &serde_json::to_vec(&snapshot).unwrap(),
            )
            .await
            .unwrap();
        context.state.circuit_breaker.record_failure(
            &repository.breaker_key(),
            crate::circuit_breaker::ProbeToken::BACKGROUND,
        );
        assert_eq!(
            index(&context, "/alias/").await.1["files"],
            serde_json::json!([])
        );
        assert!(repository
            .storage
            .stat(&super::cache::ready_key("pkg", FILE))
            .await
            .is_none());
        assert_eq!(upstream.calls.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn empty_artifacts_do_not_bypass_the_digest_verdict_via_content_length_zero() {
    let _lock = crate::test_env::env_lock();
    for bad in [false, true] {
        let upstream = source(0, Duration::ZERO, bad, false).await;
        let context = context(&upstream.uri, None, policy).await;
        let response = send(
            &context.app,
            Method::GET,
            &format!("/python/pkg/{FILE}"),
            "",
        )
        .await;
        if bad {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            not_ready(&context).await;
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            assert!(body_bytes(response).await.is_empty());
            cached(&context).await;
        }
    }
}

#[tokio::test]
async fn integrity_rules_gate_cold_bytes_and_reverify_cached_bytes_before_release() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(8, Duration::from_millis(5), false, false).await;
    let list = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(list.path(), serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"registry":"pypi","name":"pkg","version":"1.0","integrity":format!("sha256:{}", "00".repeat(32))}]})).unwrap()).unwrap();
    let blocked = context(&upstream.uri, None, |config| {
        policy(config);
        config.curation.mode = crate::config::CurationMode::Enforce;
        config.curation.allowlist_path = Some(list.path().to_str().unwrap().into());
        config.curation.require_integrity = true;
    })
    .await;
    let response = send(
        &blocked.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(upstream.sent.load(Ordering::Relaxed), 8);
    assert!(serde_json::from_slice::<serde_json::Value>(&body_bytes(response).await).is_ok());
    not_ready(&blocked).await;
    let backend = Arc::new(crate::storage::ObjectStorage::in_memory());
    let storage = crate::storage::Storage::from_backend(backend.clone());
    let seeded = context(&upstream.uri, Some(storage), policy).await;
    consume(send(&seeded.app, Method::GET, &format!("/python/pkg/{FILE}"), "").await).await;
    cached(&seeded).await;
    let bytes = vec![b'x'; 8 * 64 * 1024];
    let digest = hex::encode(sha2::Sha256::digest(&bytes));
    std::fs::write(list.path(), serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"registry":"pypi","name":"pkg","version":"1.0","integrity":format!("sha256:{digest}")}]})).unwrap()).unwrap();
    let guarded = context(
        &upstream.uri,
        Some(seeded.state.storage.clone()),
        |config| {
            policy(config);
            config.curation.mode = crate::config::CurationMode::Enforce;
            config.curation.allowlist_path = Some(list.path().to_str().unwrap().into());
            config.curation.require_integrity = true;
        },
    )
    .await;
    let repository = guarded.state.repositories.get("remote").unwrap();
    crate::storage::StorageBackend::put(
        backend.as_ref(),
        &format!(
            "repositories/remote/sources/{}/pypi/pkg/{FILE}",
            repository.source_id
        ),
        &vec![b'y'; bytes.len()],
        &digest,
    )
    .await
    .unwrap();
    let response = send(&guarded.app, Method::GET, &format!("/alias/pkg/{FILE}"), "").await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn orphan_cleanup_preserves_a_live_spool_even_after_its_mtime_becomes_old() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(8, Duration::from_millis(5), false, false).await;
    let context = context(&upstream.uri, None, policy).await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    cached(&context).await;
    settled(&context).await;
    let directory = super::transfer::temp_dir(&context.state.config.storage.path);
    let live = std::fs::read_dir(&directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let orphan = directory.join("proxy-orphan");
    std::fs::write(&orphan, b"abandoned").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(5 * 3600);
    for path in [&live, &orphan] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    super::transfer::cleanup(&context.state.config.storage.path);
    assert!(live.exists());
    assert!(!orphan.exists());
    drop(stream);
    assert!(!live.exists());
}

#[tokio::test]
async fn graceful_task_drain_can_finish_an_already_running_fill_after_client_disconnect() {
    let _lock = crate::test_env::env_lock();
    let upstream = source(8, Duration::from_millis(20), false, false).await;
    let context = context(&upstream.uri, None, policy).await;
    let response = send(
        &context.app,
        Method::GET,
        &format!("/python/pkg/{FILE}"),
        "",
    )
    .await;
    drop(response);
    context.state.background_tasks.close();
    tokio::time::timeout(
        Duration::from_secs(3),
        context.state.background_tasks.wait(),
    )
    .await
    .unwrap();
    cached(&context).await;
    assert!(!context.state.background_cancel.is_cancelled());
}
