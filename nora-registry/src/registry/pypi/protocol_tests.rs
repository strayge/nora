// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::Config;
use crate::test_helpers::{body_bytes, op_counting_storage, send_with_headers, TestContext};
use axum::body::{Body, Bytes};
use axum::http::Uri;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const FILE: &str = "pkg-1.0-py3-none-any.whl";
const DATA: &[u8] = b"0123456789";
const METADATA: &[u8] = b"Metadata-Version: 2.3\nName: pkg\nVersion: 1.0\nRequires-Dist: dep>=1\nHome-page: https://example.org/original\n\n";
const MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
const DIAGNOSTIC: &[u8] =
    b"private upstream target https://origin.invalid/file?auth=operator-secret";

type Seen = (Method, Uri, HeaderMap);
struct SourceState {
    project: RwLock<serde_json::Value>,
    requests: Mutex<Vec<Seen>>,
    down: AtomicBool,
    ignore_range: AtomicBool,
    encoded: AtomicBool,
    head_delay: AtomicU64,
    body_delay: AtomicU64,
    extra: RwLock<HeaderMap>,
}
struct Source {
    uri: String,
    state: Arc<SourceState>,
}
impl Source {
    fn calls(&self, filename: &str, method: Method) -> usize {
        self.state
            .requests
            .lock()
            .iter()
            .filter(|(seen, path, _)| {
                *seen == method && path.path() == format!("/files/{filename}")
            })
            .count()
    }
    fn file_headers(&self, filename: &str) -> Vec<HeaderMap> {
        self.state
            .requests
            .lock()
            .iter()
            .filter(|(_, path, _)| path.path() == format!("/files/{filename}"))
            .map(|(_, _, headers)| headers.clone())
            .collect()
    }
}

async fn serve(
    State(state): State<Arc<SourceState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    state
        .requests
        .lock()
        .push((method.clone(), uri.clone(), headers.clone()));
    if state.down.load(Ordering::Relaxed) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if uri.path() == "/simple/pkg/" {
        return axum::Json(state.project.read().clone()).into_response();
    }
    let metadata = uri.path() == format!("/files/{FILE}.metadata");
    if !metadata && uri.path() != format!("/files/{FILE}") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method == Method::HEAD {
        tokio::time::sleep(Duration::from_millis(
            state.head_delay.load(Ordering::Relaxed),
        ))
        .await;
    }
    let mut data = if metadata {
        METADATA.to_vec()
    } else {
        DATA.to_vec()
    };
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(&data)));
    let mut response = Response::builder()
        .header(header::ETAG, &etag)
        .header(header::LAST_MODIFIED, MODIFIED)
        .header(header::ACCEPT_RANGES, "bytes");
    let mut status = StatusCode::OK;
    if headers
        .get(header::IF_MATCH)
        .is_some_and(|value| value != etag.as_str() && value != "*")
    {
        status = StatusCode::PRECONDITION_FAILED;
    } else if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value == etag.as_str() || value == "*")
    {
        status = StatusCode::NOT_MODIFIED;
    }
    if state.encoded.load(Ordering::Relaxed) {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&data).unwrap();
        data = encoder.finish().unwrap();
        response = response.header(header::CONTENT_ENCODING, "gzip");
    }
    let size = data.len();
    if status == StatusCode::OK
        && method != Method::HEAD
        && !state.ignore_range.load(Ordering::Relaxed)
        && headers
            .get(header::IF_RANGE)
            .is_none_or(|value| value == etag.as_str())
    {
        if let Some(range) = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
        {
            match crate::registry::range::parse_byte_range(range, size as u64) {
                crate::registry::range::ParsedRange::Satisfiable(start, end) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    response = response
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
                    data = data[start as usize..=end as usize].to_vec();
                }
                crate::registry::range::ParsedRange::Unsatisfiable => {
                    status = StatusCode::RANGE_NOT_SATISFIABLE;
                    response = response.header(header::CONTENT_RANGE, format!("bytes */{size}"));
                }
                _ => {}
            }
        }
    }
    if matches!(status.as_u16(), 412 | 416) {
        data = DIAGNOSTIC.to_vec();
    }
    if matches!(status.as_u16(), 200 | 206 | 412 | 416) {
        response = response.header(header::CONTENT_LENGTH, data.len());
    }
    let body = if method == Method::HEAD || status == StatusCode::NOT_MODIFIED {
        Body::empty()
    } else {
        let delay = state.body_delay.load(Ordering::Relaxed);
        Body::from_stream(futures::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok::<_, std::io::Error>(Bytes::from(data))
        }))
    };
    let mut response = response.status(status).body(body).unwrap();
    for (name, value) in state.extra.read().iter() {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    response
}

async fn source() -> Source {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(SourceState {
        project: RwLock::new(
            serde_json::json!({"meta":{"api-version":"1.4"},"name":"pkg","versions":["1.0"],
            "files":[{"filename":FILE,"url":format!("{uri}/files/{FILE}"),"hashes":{"sha256":hex::encode(sha2::Sha256::digest(DATA))},
            "size":DATA.len(),"upload-time":"2015-10-21T07:28:00Z","requires-python":">=3.8", "core-metadata":{"sha256":hex::encode(sha2::Sha256::digest(METADATA))}}]}),
        ),
        requests: Mutex::default(),
        down: AtomicBool::new(false),
        ignore_range: AtomicBool::new(false),
        encoded: AtomicBool::new(false),
        head_delay: AtomicU64::new(0),
        body_delay: AtomicU64::new(0),
        extra: RwLock::default(),
    });
    let app = Router::new()
        .route("/{*path}", get(serve))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Source { uri, state }
}

fn policy(config: &mut Config) {
    let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
        config.repositories.get_mut("remote").unwrap()
    else {
        unreachable!()
    };
    proxy.metadata_ttl = 300;
    proxy.metadata_timeout = 2;
    proxy.read_timeout = 3;
}
async fn request(
    context: &TestContext,
    method: Method,
    filename: &str,
    headers: Vec<(&str, &str)>,
) -> Response {
    send_with_headers(
        &context.app,
        method,
        &format!("/alias/pkg/{filename}"),
        headers,
        "",
    )
    .await
}
async fn settled(context: &TestContext) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !context.state.background_tasks.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn absent(context: &TestContext, filename: &str) {
    settled(context).await;
    let repository = context.state.repositories.get("remote").unwrap();
    assert!(context
        .state
        .storage
        .stat(&format!(
            "repositories/remote/sources/{}/pypi/pkg/{filename}",
            repository.source_id
        ))
        .await
        .is_none());
    assert!(context
        .state
        .storage
        .stat(&format!(
            "repositories/remote/sources/{}/.nora-pypi-ready/pkg/{filename}.json",
            repository.source_id
        ))
        .await
        .is_none());
}

#[tokio::test]
async fn cold_head_is_only_head_and_warm_head_does_not_read_the_artifact() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let (storage, counts) = op_counting_storage();
    let context = rich_tests::context(&source.uri, Some(storage), policy).await;
    let response = request(&context, Method::HEAD, FILE, vec![("range", "bytes=0-2")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
    assert!(body_bytes(response).await.is_empty());
    assert_eq!(source.calls(FILE, Method::HEAD), 1);
    assert_eq!(source.calls(FILE, Method::GET), 0);
    assert!(!source.file_headers(FILE)[0].contains_key(header::RANGE));
    absent(&context, FILE).await;
    assert_eq!(
        body_bytes(request(&context, Method::GET, FILE, vec![]).await).await,
        DATA
    );
    settled(&context).await;
    counts.reset();
    let response = request(&context, Method::HEAD, FILE, vec![]).await;
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
    assert!(body_bytes(response).await.is_empty());
    assert_eq!(source.calls(FILE, Method::HEAD), 1);
    for method in ["get_reader", "get_range", "put", "put_from_path"] {
        assert_eq!(
            counts.snapshot().get(method).copied().unwrap_or(0),
            0,
            "{}",
            method
        );
    }
}

#[tokio::test]
async fn cold_partial_not_modified_and_unsatisfied_responses_never_publish_a_whole_file() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let context = rich_tests::context(&source.uri, None, policy).await;
    let response = request(&context, Method::GET, FILE, vec![("range", "bytes=2-4")]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
    assert_eq!(body_bytes(response).await, b"234"[..]);
    absent(&context, FILE).await;
    let response = request(&context, Method::GET, FILE, vec![("range", "bytes=10-")]).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_ne!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(DIAGNOSTIC.len().to_string().as_str())
    );
    assert!(body_bytes(response).await.is_empty());
    absent(&context, FILE).await;
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(DATA)));
    let response = request(&context, Method::GET, FILE, vec![("if-none-match", &etag)]).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(body_bytes(response).await.is_empty());
    absent(&context, FILE).await;
    let response = request(
        &context,
        Method::GET,
        FILE,
        vec![("if-match", "\"absent\"")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_ne!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(DIAGNOSTIC.len().to_string().as_str())
    );
    assert!(body_bytes(response).await.is_empty());
    absent(&context, FILE).await;
    assert_eq!(source.calls(FILE, Method::GET), 4);
}

#[tokio::test]
async fn ignored_cold_ranges_cache_only_a_verified_full_200_and_raw_encoding_is_not_decoded() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    source.state.ignore_range.store(true, Ordering::Relaxed);
    let context = rich_tests::context(&source.uri, None, policy).await;
    let response = request(&context, Method::GET, FILE, vec![("range", "bytes=2-4")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, DATA);
    settled(&context).await;
    assert_eq!(
        body_bytes(request(&context, Method::GET, FILE, vec![("range", "bytes=2-4")]).await).await,
        b"234"[..]
    );
    assert_eq!(source.calls(FILE, Method::GET), 1);
    source.state.encoded.store(true, Ordering::Relaxed);
    let raw = rich_tests::context(&source.uri, None, policy).await;
    assert_eq!(
        request(&raw, Method::GET, FILE, vec![]).await.status(),
        StatusCode::BAD_GATEWAY
    );
    absent(&raw, FILE).await;
    assert_eq!(
        source.file_headers(FILE).last().unwrap()[header::ACCEPT_ENCODING],
        "identity"
    );
}

#[tokio::test]
async fn warm_if_range_and_preconditions_are_applied_before_range_or_body_reads() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let context = rich_tests::context(&source.uri, None, policy).await;
    body_bytes(request(&context, Method::GET, FILE, vec![]).await).await;
    settled(&context).await;
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(DATA)));
    let response = request(
        &context,
        Method::GET,
        FILE,
        vec![("range", "bytes=-3"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::ETAG], etag);
    assert_eq!(body_bytes(response).await, b"789"[..]);
    let response = request(
        &context,
        Method::GET,
        FILE,
        vec![("range", "bytes=0-2"), ("if-range", "\"wrong\"")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, DATA);
    for (headers, expected) in [
        (
            vec![
                ("if-none-match", format!("\"other\", W/{etag}")),
                ("range", "bytes=0-2".into()),
            ],
            StatusCode::NOT_MODIFIED,
        ),
        (
            vec![("if-modified-since", "Thu, 22 Oct 2015 07:28:00 GMT".into())],
            StatusCode::NOT_MODIFIED,
        ),
        (
            vec![(
                "if-unmodified-since",
                "Tue, 20 Oct 2015 07:28:00 GMT".into(),
            )],
            StatusCode::PRECONDITION_FAILED,
        ),
        (
            vec![("if-match", format!("W/{etag}"))],
            StatusCode::PRECONDITION_FAILED,
        ),
        (
            vec![
                ("if-none-match", "\"wrong\"".into()),
                ("if-modified-since", "Thu, 22 Oct 2015 07:28:00 GMT".into()),
            ],
            StatusCode::OK,
        ),
    ] {
        let response = request(
            &context,
            Method::GET,
            FILE,
            headers
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect(),
        )
        .await;
        assert_eq!(response.status(), expected);
        let _ = body_bytes(response).await;
    }
    let (storage, counts) = op_counting_storage();
    let counted = rich_tests::context(&source.uri, Some(storage), policy).await;
    body_bytes(request(&counted, Method::GET, FILE, vec![]).await).await;
    settled(&counted).await;
    counts.reset();
    let response = request(&counted, Method::GET, FILE, vec![("range", "bytes=10-")]).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    assert_eq!(response.headers()[header::ETAG], etag);
    assert_eq!(response.headers()[header::LAST_MODIFIED], MODIFIED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(body_bytes(response).await.is_empty());
    assert!(!counts.snapshot().contains_key("get_reader"));
    assert!(!counts.snapshot().contains_key("get_range"));
    assert_eq!(source.calls(FILE, Method::GET), 2);
}

#[tokio::test]
async fn head_has_a_metadata_budget_but_sidecar_bodies_use_the_independent_file_budget() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    source.state.head_delay.store(1500, Ordering::Relaxed);
    let context = rich_tests::context(&source.uri, None, |config| {
        policy(config);
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.metadata_timeout = 1;
    })
    .await;
    let start = std::time::Instant::now();
    assert_eq!(
        request(&context, Method::HEAD, FILE, vec![]).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(start.elapsed() < Duration::from_millis(1400));
    assert_eq!(source.calls(FILE, Method::GET), 0);
    absent(&context, FILE).await;
    source.state.body_delay.store(1500, Ordering::Relaxed);
    let context = rich_tests::context(&source.uri, None, |config| {
        policy(config);
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.metadata_timeout = 1;
        proxy.total_timeout = 0;
    })
    .await;
    let start = std::time::Instant::now();
    assert_eq!(
        body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    assert!(start.elapsed() > Duration::from_millis(1400));
    assert_eq!(source.calls(FILE, Method::GET), 0);
}

#[tokio::test]
async fn advertised_sidecars_are_opaque_parent_bound_and_metadata_only_then_survive_offline_restart(
) {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let context = rich_tests::context(&source.uri, None, policy).await;
    let (_, project) = rich_tests::index(&context, "/python/").await;
    assert_eq!(
        project["files"][0]["core-metadata"]["sha256"],
        hex::encode(sha2::Sha256::digest(METADATA))
    );
    let response = request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, METADATA);
    settled(&context).await;
    assert_eq!(source.calls(FILE, Method::GET), 0);
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
    assert_eq!(
        body_bytes(request(&context, Method::GET, FILE, vec![]).await).await,
        DATA
    );
    settled(&context).await;
    source.state.down.store(true, Ordering::Relaxed);
    let restarted =
        rich_tests::context(&source.uri, Some(context.state.storage.clone()), |config| {
            policy(config);
            let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
                config.repositories.get_mut("remote").unwrap()
            else {
                unreachable!()
            };
            proxy.metadata_ttl = 0;
            config.circuit_breaker.enabled = true;
            config.circuit_breaker.failure_threshold = 1;
            config.circuit_breaker.reset_timeout = 3600;
        })
        .await;
    let (_, offline) = rich_tests::index(&restarted, "/alias/").await;
    assert_eq!(offline["files"].as_array().unwrap().len(), 1);
    assert!(offline["files"][0]["core-metadata"].is_object());
    assert_eq!(
        body_bytes(request(&restarted, Method::GET, &format!("{FILE}.metadata"), vec![]).await)
            .await,
        METADATA
    );
    let repository = restarted.state.repositories.get("remote").unwrap();
    let prefix = format!("repositories/remote/sources/{}/", repository.source_id);
    restarted
        .state
        .storage
        .delete(&format!("{prefix}pypi/pkg/{FILE}.metadata"))
        .await
        .unwrap();
    restarted
        .state
        .storage
        .delete(&format!(
            "{prefix}.nora-pypi-ready/pkg/{FILE}.metadata.json"
        ))
        .await
        .unwrap();
    let (_, offline) = rich_tests::index(&restarted, "/python/").await;
    assert_eq!(offline["files"].as_array().unwrap().len(), 1);
    assert!(offline["files"][0].get("core-metadata").is_none());
    assert_eq!(source.calls(FILE, Method::GET), 1);
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
    source.state.down.store(false, Ordering::Relaxed);
    let recovered =
        rich_tests::context(&source.uri, Some(restarted.state.storage.clone()), policy).await;
    let (_, online) = rich_tests::index(&recovered, "/alias/").await;
    assert!(online["files"][0]["core-metadata"].is_object());
}

#[tokio::test]
async fn sidecars_cannot_use_unadvertised_or_foreign_source_bytes() {
    let _lock = crate::test_env::env_lock();
    let first = source().await;
    let original = rich_tests::context(&first.uri, None, policy).await;
    assert_eq!(
        body_bytes(request(&original, Method::GET, &format!("{FILE}.metadata"), vec![]).await)
            .await,
        METADATA
    );
    settled(&original).await;
    let second = source().await;
    second.state.project.write()["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("core-metadata");
    let changed =
        rich_tests::context(&second.uri, Some(original.state.storage.clone()), policy).await;
    assert_eq!(
        request(&changed, Method::GET, &format!("{FILE}.metadata"), vec![])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(second.calls(&format!("{FILE}.metadata"), Method::GET), 0);
    second.state.project.write()["files"][0]["core-metadata"] =
        serde_json::json!({"sha256":hex::encode(sha2::Sha256::digest(METADATA))});
    let available =
        rich_tests::context(&second.uri, Some(changed.state.storage.clone()), |config| {
            policy(config);
            let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
                config.repositories.get_mut("remote").unwrap()
            else {
                unreachable!()
            };
            proxy.metadata_ttl = 0;
        })
        .await;
    rich_tests::index(&available, "/alias/").await;
    second.state.down.store(true, Ordering::Relaxed);
    assert_eq!(
        request(&available, Method::GET, &format!("{FILE}.metadata"), vec![])
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        request(&changed, Method::GET, "dates.json.metadata", vec![])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn parent_integrity_policy_needs_a_current_parent_proof_and_never_hashes_sidecar_bytes_as_the_wheel(
) {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let list = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(list.path(), serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"registry":"pypi","name":"pkg","version":"1.0","integrity":format!("sha256:{}", hex::encode(sha2::Sha256::digest(DATA)))}]})).unwrap()).unwrap();
    let context = rich_tests::context(&source.uri, None, |config| {
        policy(config);
        config.curation.mode = crate::config::CurationMode::Enforce;
        config.curation.require_integrity = true;
        config.curation.allowlist_path = Some(list.path().to_str().unwrap().into());
    })
    .await;
    let (_, project) = rich_tests::index(&context, "/python/").await;
    assert!(project["files"][0].get("core-metadata").is_none());
    assert_eq!(
        request(&context, Method::GET, &format!("{FILE}.metadata"), vec![])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(source.calls(FILE, Method::GET), 0);
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 0);
    let response = request(&context, Method::GET, FILE, vec![("range", "bytes=0-2")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, DATA);
    settled(&context).await;
    assert!(!source.file_headers(FILE)[0].contains_key(header::RANGE));
    let (_, project) = rich_tests::index(&context, "/alias/").await;
    assert!(project["files"][0]["core-metadata"].is_object());
    assert_eq!(
        body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    settled(&context).await;
    std::fs::write(list.path(), serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"registry":"pypi","name":"pkg","version":"1.0","integrity":format!("sha256:{}", "00".repeat(32))}]})).unwrap()).unwrap();
    let mismatched =
        rich_tests::context(&source.uri, Some(context.state.storage.clone()), |config| {
            policy(config);
            config.curation.mode = crate::config::CurationMode::Enforce;
            config.curation.require_integrity = true;
            config.curation.allowlist_path = Some(list.path().to_str().unwrap().into());
        })
        .await;
    let (_, project) = rich_tests::index(&mismatched, "/alias/").await;
    assert!(project["files"][0].get("core-metadata").is_none());
    assert_eq!(
        request(
            &mismatched,
            Method::GET,
            &format!("{FILE}.metadata"),
            vec![]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
}

#[tokio::test]
async fn conflicting_partial_headers_do_not_become_cache_readiness() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    for ignored in [false, true] {
        source.state.ignore_range.store(ignored, Ordering::Relaxed);
        source.state.extra.write().insert(
            header::CONTENT_RANGE,
            if ignored {
                axum::http::HeaderValue::from_static("bytes 0-2/10")
            } else {
                axum::http::HeaderValue::from_static("bytes 0-2/99")
            },
        );
        let context = rich_tests::context(&source.uri, None, policy).await;
        assert_eq!(
            request(&context, Method::GET, FILE, vec![("range", "bytes=0-2")])
                .await
                .status(),
            StatusCode::BAD_GATEWAY
        );
        absent(&context, FILE).await;
    }
}

#[tokio::test]
async fn refreshed_sidecar_hashes_and_parent_bindings_retire_old_cached_metadata() {
    let _lock = crate::test_env::env_lock();
    for field in ["core-metadata", "hashes"] {
        let source = source().await;
        let context = rich_tests::context(&source.uri, None, |config| {
            policy(config);
            let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
                config.repositories.get_mut("remote").unwrap()
            else {
                unreachable!()
            };
            proxy.metadata_ttl = 0;
        })
        .await;
        assert_eq!(
            body_bytes(request(&context, Method::GET, FILE, vec![]).await).await,
            DATA
        );
        assert_eq!(
            body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await)
                .await,
            METADATA
        );
        settled(&context).await;
        source.state.project.write()["files"][0][field] =
            serde_json::json!({"sha256":"11".repeat(32)});
        rich_tests::index(&context, "/python/").await;
        source.state.down.store(true, Ordering::Relaxed);
        let (_, offline) = rich_tests::index(&context, "/alias/").await;
        if field == "core-metadata" {
            assert_eq!(offline["files"].as_array().unwrap().len(), 1);
            assert!(offline["files"][0].get("core-metadata").is_none());
        } else {
            assert!(offline["files"].as_array().unwrap().is_empty());
        }
        assert_eq!(
            request(&context, Method::GET, &format!("{FILE}.metadata"), vec![])
                .await
                .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
    }
}

#[tokio::test]
async fn sidecar_source_urls_extend_the_path_without_mutating_private_queries() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    source.state.project.write()["files"][0]["url"] = serde_json::json!(format!(
        "{}/files/{FILE}?token=private&sig=a%2Fb#sha256={}",
        source.uri,
        hex::encode(sha2::Sha256::digest(DATA))
    ));
    let context = rich_tests::context(&source.uri, None, policy).await;
    let (_, project) = rich_tests::index(&context, "/python/").await;
    assert!(!project["files"][0]["url"]
        .as_str()
        .unwrap()
        .contains("private"));
    assert_eq!(
        body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    let requests = source.state.requests.lock();
    let (_, target, _) = requests
        .iter()
        .find(|(_, uri, _)| uri.path() == format!("/files/{FILE}.metadata"))
        .unwrap();
    assert_eq!(target.query(), Some("token=private&sig=a%2Fb"));
    drop(requests);
    assert_eq!(source.calls(FILE, Method::GET), 0);
}

#[tokio::test]
async fn unknown_size_ranges_validate_offsets_and_keep_base_json_metadata_without_newer_fields() {
    let _lock = crate::test_env::env_lock();
    for (requested, returned, expected) in [
        ("bytes=0-2", "bytes 0-2/*", StatusCode::PARTIAL_CONTENT),
        ("bytes=0-2", "bytes 5-7/*", StatusCode::BAD_GATEWAY),
        ("bytes=2-", "bytes 2-9/*", StatusCode::PARTIAL_CONTENT),
        ("bytes=-3", "bytes 7-9/*", StatusCode::BAD_GATEWAY),
    ] {
        let source = source().await;
        {
            let mut project = source.state.project.write();
            project["meta"]["api-version"] = serde_json::json!("1.0");
            project.as_object_mut().unwrap().remove("versions");
            let file = project["files"][0].as_object_mut().unwrap();
            file.remove("size");
            file.remove("upload-time");
        }
        source.state.extra.write().insert(
            header::CONTENT_RANGE,
            axum::http::HeaderValue::from_str(returned).unwrap(),
        );
        let context = rich_tests::context(&source.uri, None, policy).await;
        let (_, project) = rich_tests::index(&context, "/python/").await;
        assert_eq!(project["meta"]["api-version"], "1.0");
        assert!(project.get("versions").is_none());
        assert!(project["files"][0].get("size").is_none());
        assert!(project["files"][0].get("upload-time").is_none());
        assert!(project["files"][0]["core-metadata"].is_object());
        let html = send_with_headers(
            &context.app,
            Method::GET,
            "/alias/pkg/",
            vec![("accept", "text/html")],
            "",
        )
        .await;
        assert!(String::from_utf8_lossy(&body_bytes(html).await).contains("data-core-metadata="));
        let response = request(&context, Method::GET, FILE, vec![("range", requested)]).await;
        assert_eq!(response.status(), expected, "{requested}: {returned}");
        if expected == StatusCode::PARTIAL_CONTENT {
            assert_eq!(response.headers()[header::CONTENT_RANGE], returned);
            assert!(!body_bytes(response).await.is_empty());
        }
        absent(&context, FILE).await;
        assert_eq!(source.calls(FILE, Method::HEAD), 0);
    }
}

#[tokio::test]
async fn unsaved_files_and_sidecars_keep_metadata_and_security_records_but_not_offline_candidates()
{
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let configure = |config: &mut Config| {
        policy(config);
        config.server.trust_upstream_dates = false;
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.save_files = false;
        proxy.metadata_ttl = 0;
        config.curation.pypi.quarantine = Some(crate::digest_quarantine::QuarantineMode::Observe);
        config.curation.pypi.quarantine_ttl = Some("1h".into());
        config.circuit_breaker.enabled = true;
        config.circuit_breaker.failure_threshold = 1;
        config.circuit_breaker.reset_timeout = 3600;
    };
    let context = rich_tests::context(&source.uri, None, configure).await;
    assert_eq!(
        body_bytes(request(&context, Method::GET, FILE, vec![]).await).await,
        DATA
    );
    assert_eq!(
        body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    absent(&context, FILE).await;
    absent(&context, &format!("{FILE}.metadata")).await;
    for data in [DATA, METADATA] {
        assert!(!matches!(
            context.state.digest_store.check(
                "pypi",
                &format!("sha256:{}", hex::encode(sha2::Sha256::digest(data))),
                3600
            ),
            crate::digest_quarantine::QuarantineStatus::New
        ));
    }
    let repository = context.state.repositories.get("remote").unwrap();
    assert!(named::stored(&repository.state(&context.state), "pkg")
        .await
        .unwrap()
        .unwrap()
        .project
        .is_some());
    source.state.down.store(true, Ordering::Relaxed);
    let (_, offline) = rich_tests::index(&context, "/alias/").await;
    assert!(offline["files"].as_array().unwrap().is_empty());
    assert_eq!(
        request(&context, Method::GET, FILE, vec![]).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        request(&context, Method::GET, &format!("{FILE}.metadata"), vec![])
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(source.calls(FILE, Method::GET), 1);
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
}

#[tokio::test]
async fn mixed_save_policies_and_flag_changes_keep_old_ready_files_and_local_publication() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let configure = |config: &mut Config| {
        policy(config);
        config.repositories.extend(
            toml::from_str::<Config>(&format!(
                r#"
[repositories.transient]
format="pypi"
kind="proxy"
paths=["/transient/"]
upstream="{}/simple"
save_files=false
[repositories.local]
format="pypi"
kind="local"
paths=["/local/"]
"#,
                source.uri
            ))
            .unwrap()
            .repositories,
        );
    };
    let context = rich_tests::context(&source.uri, None, configure).await;
    assert_eq!(
        body_bytes(request(&context, Method::GET, FILE, vec![]).await).await,
        DATA
    );
    assert_eq!(
        body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    let response = send_with_headers(
        &context.app,
        Method::GET,
        &format!("/transient/pkg/{FILE}"),
        vec![],
        "",
    )
    .await;
    assert_eq!(body_bytes(response).await, DATA);
    settled(&context).await;
    let transient = context.state.repositories.get("transient").unwrap();
    assert!(transient
        .storage
        .stat(&format!("pypi/pkg/{FILE}"))
        .await
        .is_none());
    assert!(transient
        .storage
        .stat(&super::cache::ready_key("pkg", FILE))
        .await
        .is_none());
    source.state.down.store(true, Ordering::Relaxed);
    let changed = rich_tests::context(&source.uri, Some(context.state.storage.clone()), |config| {
        configure(config);
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.save_files = false;
    })
    .await;
    assert_eq!(
        body_bytes(request(&changed, Method::GET, FILE, vec![]).await).await,
        DATA
    );
    assert_eq!(
        body_bytes(request(&changed, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await,
        METADATA
    );
    let content_type = format!(
        "multipart/form-data; boundary={}",
        super::local_index_cost_tests::BOUNDARY
    );
    let uploaded = send_with_headers(
        &changed.app,
        Method::POST,
        "/local/",
        vec![("content-type", &content_type)],
        super::local_index_cost_tests::upload_body("pkg", "1.0", FILE, DATA),
    )
    .await;
    assert_eq!(uploaded.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(
            send_with_headers(
                &changed.app,
                Method::GET,
                &format!("/local/pkg/{FILE}"),
                vec![],
                ""
            )
            .await
        )
        .await,
        DATA
    );
    let head = send_with_headers(
        &changed.app,
        Method::HEAD,
        &format!("/local/pkg/{FILE}"),
        vec![],
        "",
    )
    .await;
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "10");
    assert!(body_bytes(head).await.is_empty());
    assert!(changed
        .state
        .repositories
        .get("local")
        .unwrap()
        .storage
        .stat(&format!("pypi/pkg/{FILE}"))
        .await
        .is_some());
    assert_eq!(source.calls(FILE, Method::GET), 2);
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
}

#[tokio::test]
async fn known_project_quarantine_blocks_cached_sidecars_head_range_aliases_and_restart() {
    let _lock = crate::test_env::env_lock();
    let source = source().await;
    let context = rich_tests::context(&source.uri, None, |config| {
        policy(config);
        let crate::config::repositories::RepositoryConfig::Proxy(proxy) =
            config.repositories.get_mut("remote").unwrap()
        else {
            unreachable!()
        };
        proxy.metadata_ttl = 0;
    })
    .await;
    body_bytes(request(&context, Method::GET, &format!("{FILE}.metadata"), vec![]).await).await;
    settled(&context).await;
    source.state.project.write()["project-status"] =
        serde_json::json!({"status":"quarantined","reason":"held"});
    let (_, held) = rich_tests::index(&context, "/python/").await;
    assert!(held["files"].as_array().unwrap().is_empty());
    for method in [Method::GET, Method::HEAD] {
        assert_eq!(
            request(
                &context,
                method,
                &format!("{FILE}.metadata"),
                vec![("range", "bytes=0-2")]
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    source.state.down.store(true, Ordering::Relaxed);
    let restarted =
        rich_tests::context(&source.uri, Some(context.state.storage.clone()), policy).await;
    assert_eq!(
        request(&restarted, Method::GET, &format!("{FILE}.metadata"), vec![])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(source.calls(&format!("{FILE}.metadata"), Method::GET), 1);
}
