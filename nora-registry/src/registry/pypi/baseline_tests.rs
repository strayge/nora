// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Characterize the singleton transfer paths before named repository integration.

use super::hash_marker_key;
use super::hash_marker_tests::wait_for_markers;
use crate::storage::{ObjectStorage, Storage, StorageBackend};
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, create_test_context_with_storage_and_config,
    op_counting_storage, send, send_with_headers,
};
use axum::http::{Method, StatusCode};
use sha2::Digest;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FILENAME: &str = "baseline-1.0.tar.gz";
const DOWNLOAD: &str = "/simple/baseline/baseline-1.0.tar.gz";

async fn upstream(content: &[u8]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/baseline/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<a href=\"{}/files/{FILENAME}\">{FILENAME}</a>",
            server.uri()
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILENAME}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
        .mount(&server)
        .await;
    server
}

/// Cold HEAD fetches the entire body via GET; a cold Range is not forwarded.
#[tokio::test]
async fn implicit_head_and_range_miss_pay_for_a_full_upstream_get() {
    let content = b"0123456789abcdef";
    for request_method in [Method::HEAD, Method::GET] {
        let upstream = upstream(content).await;
        let ctx = create_test_context_with_config(|cfg| {
            cfg.pypi.proxy = Some(upstream.uri());
            cfg.server.trust_upstream_dates = false;
        });
        let response = send_with_headers(
            &ctx.app,
            request_method.clone(),
            DOWNLOAD,
            vec![("range", "bytes=2-5")],
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("content-range").is_none());
        let body = body_bytes(response).await;
        assert_eq!(
            body.as_ref(),
            if request_method == Method::HEAD {
                &b""[..]
            } else {
                &content[..]
            }
        );
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one HTML index and one full artifact GET"
        );
        let file = requests
            .iter()
            .find(|r| r.url.path().starts_with("/files/"))
            .unwrap();
        assert_eq!(file.method.as_str(), "GET");
        assert!(!file.headers.contains_key("range"));
        let sha = hex::encode(sha2::Sha256::digest(content));
        wait_for_markers(
            &ctx,
            "baseline",
            &[hash_marker_key("baseline", FILENAME, &sha)],
        )
        .await;
        assert_eq!(
            ctx.state
                .storage
                .get(&format!("pypi/baseline/{FILENAME}"))
                .await
                .unwrap()
                .as_ref(),
            content
        );
    }
}

/// A stalled metadata response consumes two full request timeouts plus retry backoff.
#[tokio::test]
async fn stalled_index_retries_its_timeout_and_returns_an_empty_502() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&upstream)
        .await;
    let ctx = create_test_context_with_config(|cfg| {
        cfg.pypi.proxy = Some(upstream.uri());
        cfg.pypi.proxy_timeout = 1;
        cfg.server.trust_upstream_dates = false;
    });
    let start = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(&ctx.app, Method::GET, DOWNLOAD, ""),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(body_bytes(response).await.is_empty());
    assert!(start.elapsed() >= Duration::from_secs(3));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert!(ctx
        .state
        .storage
        .stat(&format!("pypi/baseline/{FILENAME}"))
        .await
        .is_none());
}

/// The current buffered path delivers no headers or partial bytes before a stalled body fails.
#[tokio::test]
async fn stalled_file_body_times_out_before_client_response_without_body_retry() {
    use axum::{
        body::{Body, Bytes},
        routing::get,
        Router,
    };
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let file_gets = Arc::new(AtomicUsize::new(0));
    let gets = Arc::clone(&file_gets);
    let app = Router::new()
        .route(
            "/baseline/",
            get(|| async { format!("<a href=\"/files/{FILENAME}\">{FILENAME}</a>") }),
        )
        .route(
            &format!("/files/{FILENAME}"),
            get(move || {
                gets.fetch_add(1, Ordering::SeqCst);
                async {
                    Body::from_stream(
                        futures::stream::once(async {
                            Ok::<_, std::io::Error>(Bytes::from_static(b"partial"))
                        })
                        .chain(futures::stream::pending()),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ctx = create_test_context_with_config(|cfg| {
        cfg.pypi.proxy = Some(format!("http://{address}"));
        cfg.pypi.proxy_timeout = 1;
        cfg.server.trust_upstream_dates = false;
    });
    let start = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send(&ctx.app, Method::GET, DOWNLOAD, ""),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(body_bytes(response).await.is_empty());
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert_eq!(file_gets.load(Ordering::SeqCst), 1);
    assert!(ctx
        .state
        .storage
        .stat(&format!("pypi/baseline/{FILENAME}"))
        .await
        .is_none());
    server.abort();
}

/// A failed asynchronous fill does not revoke a successful download or publish hash markers.
#[tokio::test]
async fn failed_background_cache_write_still_returns_downloaded_bytes() {
    let upstream = upstream(b"uncached bytes").await;
    let failed = Arc::new(tokio::sync::Notify::new());
    let storage = Storage::from_backend(Arc::new(FailingWrites {
        inner: ObjectStorage::in_memory(),
        failed: Arc::clone(&failed),
    }));
    let ctx = create_test_context_with_storage_and_config(storage, |cfg| {
        cfg.pypi.proxy = Some(upstream.uri());
        cfg.server.trust_upstream_dates = false;
    });
    for _ in 0..2 {
        let response = send(&ctx.app, Method::GET, DOWNLOAD, "").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await.as_ref(), b"uncached bytes");
        tokio::time::timeout(Duration::from_secs(5), failed.notified())
            .await
            .unwrap();
        assert!(ctx.state.storage.list("pypi/").await.unwrap().is_empty());
        assert!(ctx
            .state
            .storage
            .list("pypi-sha256/")
            .await
            .unwrap()
            .is_empty());
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 4);
}

/// Separate cold contexts see different sources, but the singleton warm key ignores source.
#[tokio::test]
async fn same_filename_different_upstreams_exposes_legacy_cache_sharing() {
    let first = upstream(b"first source").await;
    let second = upstream(b"second source").await;
    let (storage, _) = op_counting_storage();
    let ctx = create_test_context_with_storage_and_config(storage.clone(), |cfg| {
        cfg.pypi.proxy = Some(first.uri());
        cfg.server.trust_upstream_dates = false;
    });
    let response = send(&ctx.app, Method::GET, DOWNLOAD, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"first source");
    let sha = hex::encode(sha2::Sha256::digest(b"first source"));
    wait_for_markers(
        &ctx,
        "baseline",
        &[hash_marker_key("baseline", FILENAME, &sha)],
    )
    .await;

    let warm = create_test_context_with_storage_and_config(storage, |cfg| {
        cfg.pypi.proxy = Some(second.uri());
        cfg.server.trust_upstream_dates = false;
    });
    let response = send(&warm.app, Method::GET, DOWNLOAD, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"first source");
    assert!(second.received_requests().await.unwrap().is_empty());

    let cold = create_test_context_with_config(|cfg| {
        cfg.pypi.proxy = Some(second.uri());
        cfg.server.trust_upstream_dates = false;
    });
    let response = send(&cold.app, Method::GET, DOWNLOAD, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"second source");
}

struct FailingWrites {
    inner: ObjectStorage,
    failed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl StorageBackend for FailingWrites {
    async fn put(&self, _: &str, _: &[u8], _: &str) -> crate::storage::Result<()> {
        self.failed.notify_one();
        Err(crate::storage::StorageError::Network(
            "injected write failure".into(),
        ))
    }
    async fn get(&self, key: &str) -> crate::storage::Result<(axum::body::Bytes, Option<String>)> {
        self.inner.get(key).await
    }
    async fn pin(&self, key: &str) -> Option<String> {
        self.inner.pin(key).await
    }
    async fn delete(&self, key: &str) -> crate::storage::Result<()> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> crate::storage::Result<Vec<String>> {
        self.inner.list(prefix).await
    }
    async fn stat(&self, key: &str) -> Option<crate::storage::FileMeta> {
        self.inner.stat(key).await
    }
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
    async fn total_size(&self) -> u64 {
        self.inner.total_size().await
    }
    fn backend_name(&self) -> &'static str {
        "failing-writes-test"
    }
    async fn put_from_path(
        &self,
        key: &str,
        src: &std::path::Path,
        sha: Option<&str>,
    ) -> crate::storage::Result<()> {
        self.inner.put_from_path(key, src, sha).await
    }
    async fn copy(&self, src: &str, dst: &str, sha: Option<&str>) -> crate::storage::Result<()> {
        self.inner.copy(src, dst, sha).await
    }
    async fn get_reader(
        &self,
        key: &str,
    ) -> crate::storage::Result<(
        u64,
        Option<String>,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    )> {
        self.inner.get_reader(key).await
    }
}
