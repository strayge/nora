// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Transfer completion and durable cache readiness are deliberately different events.

use super::{cache, model, named, *};
use crate::registry::spool::{spool_follower, SpoolFile, SpoolStatus};
use crate::registry::transport::ProxyClient;
use crate::registry::{ProxyError, TempFileGuard, Validators};
use axum::body::{Body, Bytes};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::digest::DynDigest;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

static ACTIVE: LazyLock<parking_lot::Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
const CHUNK: usize = 256 * 1024;

struct ActiveTemp {
    path: PathBuf,
    _guard: TempFileGuard,
}
impl ActiveTemp {
    fn new(path: PathBuf, guard: TempFileGuard) -> Self {
        ACTIVE.lock().insert(path.clone());
        Self {
            path,
            _guard: guard,
        }
    }
}
impl Drop for ActiveTemp {
    fn drop(&mut self) {
        ACTIVE.lock().remove(&self.path);
    }
}

pub(super) fn temp_dir(data: &str) -> PathBuf {
    FsPath::new(data).join("tmp/pypi-proxy")
}

pub(super) fn cleanup(data: &str) {
    let Ok(entries) = std::fs::read_dir(temp_dir(data)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("proxy-")
            || ACTIVE.lock().contains(&path)
        {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(4 * 3600));
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Clone)]
struct Failure {
    status: StatusCode,
    headers: Arc<HeaderMap>,
    body: Bytes,
}
impl Failure {
    async fn response(response: Response) -> Self {
        let (parts, body) = response.into_parts();
        match axum::body::to_bytes(body, 64 * 1024).await {
            Ok(body) => Self {
                status: parts.status,
                headers: Arc::new(parts.headers),
                body,
            },
            Err(_) => Self::status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: Arc::new(HeaderMap::new()),
            body: Bytes::new(),
        }
    }
    async fn error(error: ProxyError) -> Self {
        if matches!(error, ProxyError::Local(_)) {
            return Self::status(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Self::response(named::failure(error)).await
    }
    fn into_response(self) -> Response {
        let mut response = (self.status, self.body).into_response();
        *response.headers_mut() = self.headers.as_ref().clone();
        response
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Head {
    length: Option<u64>,
    validators: Validators,
    cached: bool,
}
#[derive(Clone)]
struct Complete {
    size: u64,
    hashes: BTreeMap<String, String>,
    head: Head,
}
#[derive(Clone, Default)]
struct Progress {
    head: Option<Head>,
    result: Option<Result<Complete, Failure>>,
}

pub(crate) struct Transfer {
    path: PathBuf,
    _owner: Arc<ActiveTemp>,
    status: watch::Receiver<SpoolStatus>,
    progress: watch::Receiver<Progress>,
}

struct Verdict {
    status: watch::Sender<SpoolStatus>,
    progress: watch::Sender<Progress>,
}
impl Drop for Verdict {
    fn drop(&mut self) {
        self.status.send_modify(|status| {
            if status.done.is_none() {
                status.done = Some(Err("artifact transfer abandoned".into()));
            }
        });
        self.progress.send_modify(|progress| {
            if progress.result.is_none() {
                progress.result = Some(Err(Failure::status(StatusCode::BAD_GATEWAY)));
            }
        });
    }
}

fn quarantine(state: &AppState) -> (crate::digest_quarantine::QuarantineMode, i64) {
    crate::digest_quarantine::resolve_global(
        state.config.curation.pypi.quarantine.as_ref().or(state
            .config
            .curation
            .quarantine
            .as_ref()),
        state
            .config
            .curation
            .pypi
            .quarantine_ttl
            .as_deref()
            .or(state.config.curation.quarantine_ttl.as_deref()),
    )
}
fn read_timeout(state: &AppState) -> Duration {
    let seconds = state
        .repository
        .as_ref()
        .and_then(|repository| match &repository.definition {
            crate::config::repositories::RepositoryConfig::Proxy(policy) => {
                Some(policy.read_timeout)
            }
            _ => None,
        })
        .unwrap_or(state.config.pypi.proxy_timeout);
    Duration::from_secs(seconds)
}

fn record_serve(state: &AppState, name: &str, filename: &str, cached: bool) {
    state.metrics.record_download("pypi");
    if cached {
        state.metrics.record_cache_hit("pypi");
    } else {
        state.metrics.record_cache_miss("pypi");
    }
    state.activity.push(ActivityEntry::new(
        if cached {
            ActionType::CacheHit
        } else {
            ActionType::ProxyFetch
        },
        format!("{name}/{filename}"),
        RegistryType::PyPI,
        if cached { "CACHE" } else { "PROXY" },
    ));
    state.audit.log(AuditEntry::new(
        if cached { "cache_hit" } else { "proxy_fetch" },
        "proxy",
        "",
        "pypi",
        "",
    ));
}

fn full_gate(state: &AppState, name: &str, filename: &str) -> bool {
    matches!(
        quarantine(state).0,
        crate::digest_quarantine::QuarantineMode::Enforce
    ) || crate::curation::integrity_applies(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, filename).as_deref(),
    )
}
async fn policy(
    state: &AppState,
    name: &str,
    file: &model::Distribution,
    sha256: &str,
    date: Option<i64>,
) -> Option<Failure> {
    let digest = format!("sha256:{sha256}");
    if let Some(response) = crate::curation::verify_integrity_by_hash(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &file.filename).as_deref(),
        &digest,
    ) {
        return Some(Failure::response(response).await);
    }
    let (mode, seconds) = quarantine(state);
    let origin = state
        .repository
        .as_ref()
        .map(|repository| repository.breaker_key())
        .unwrap_or_else(|| "cache".into());
    if let Some(response) = crate::digest_quarantine::proxy_gate_hash_dated(
        &state.digest_store,
        "pypi",
        &digest,
        &mode,
        seconds,
        &origin,
        date,
    ) {
        Some(Failure::response(response).await)
    } else {
        None
    }
}

pub(super) fn hashers(file: &model::Distribution) -> Vec<(String, Box<dyn DynDigest + Send>)> {
    let mut algorithms = file
        .hashes
        .keys()
        .filter(|algorithm| cache::supported(algorithm))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    algorithms.insert("sha256".into());
    algorithms
        .into_iter()
        .filter_map(|algorithm| {
            let hasher: Box<dyn DynDigest + Send> = match algorithm.as_str() {
                "sha224" => Box::new(sha2::Sha224::default()),
                "sha256" => Box::new(sha2::Sha256::default()),
                "sha384" => Box::new(sha2::Sha384::default()),
                "sha512" => Box::new(sha2::Sha512::default()),
                "sha1" => Box::new(sha1::Sha1::default()),
                "md5" => Box::new(md5::Md5::default()),
                _ => return None,
            };
            Some((algorithm, hasher))
        })
        .collect()
}

async fn copy<S>(
    stream: S,
    writer: &mut tokio::fs::File,
    file: &model::Distribution,
    length: Option<u64>,
    status: &watch::Sender<SpoolStatus>,
) -> Result<(u64, BTreeMap<String, String>), ProxyError>
where
    S: Stream<Item = Result<Bytes, ProxyError>>,
{
    let mut stream = Box::pin(stream);
    let mut hashes = hashers(file);
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ProxyError::Network("artifact length overflow".into()))?;
        if length.is_some_and(|length| written > length)
            || file.size.is_some_and(|size| written > size)
        {
            return Err(ProxyError::Network(
                "artifact exceeds its declared size".into(),
            ));
        }
        for (_, hasher) in &mut hashes {
            hasher.update(&chunk);
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|_| ProxyError::Local("artifact spool write failed".into()))?;
        writer
            .flush()
            .await
            .map_err(|_| ProxyError::Local("artifact spool flush failed".into()))?;
        status.send_modify(|status| status.written = written);
    }
    if length.is_some_and(|length| length != written)
        || file.size.is_some_and(|size| size != written)
    {
        return Err(ProxyError::Network("artifact body truncated".into()));
    }
    writer
        .sync_all()
        .await
        .map_err(|_| ProxyError::Local("artifact spool sync failed".into()))?;
    let hashes = hashes
        .into_iter()
        .map(|(algorithm, hasher)| (algorithm, hex::encode(hasher.finalize())))
        .collect();
    if !cache::expected_match(file, written, &hashes) {
        return Err(ProxyError::Network("artifact digest mismatch".into()));
    }
    Ok((written, hashes))
}

#[derive(Clone)]
enum Input {
    Upstream,
    Stored {
        size: u64,
        sha256: String,
        validators: Validators,
    },
}

async fn fill(
    state: &AppState,
    name: &str,
    file: &model::Distribution,
    date: Option<i64>,
    input: Input,
    writer: &mut tokio::fs::File,
    verdict: &Verdict,
) -> Result<Result<Complete, Failure>, ProxyError> {
    let input = if matches!(input, Input::Upstream) {
        match cache::load(state, name, file, None).await? {
            Some(ready) => Input::Stored {
                size: ready.size,
                sha256: ready
                    .hashes
                    .get("sha256")
                    .cloned()
                    .ok_or_else(|| ProxyError::Local("missing cached digest".into()))?,
                validators: ready.validators,
            },
            None => input,
        }
    } else {
        input
    };
    match input {
        Input::Stored {
            size,
            sha256,
            validators,
        } => {
            let (length, pin, reader) = state
                .storage
                .get_reader(&cache::body_key(name, &file.filename))
                .await
                .map_err(|_| ProxyError::Local("cached artifact unavailable".into()))?;
            if length != size || pin.as_ref() != Some(&sha256) {
                return Err(ProxyError::Local("cached artifact changed".into()));
            }
            let head = Head {
                length: Some(size),
                validators,
                cached: true,
            };
            verdict
                .progress
                .send_modify(|progress| progress.head = Some(head.clone()));
            let stream = tokio_util::io::ReaderStream::with_capacity(reader, CHUNK)
                .map(|result| result.map_err(|_| ProxyError::Local("cache read failed".into())));
            let (size, hashes) = copy(stream, writer, file, Some(size), &verdict.status).await?;
            if hashes.get("sha256") != Some(&sha256) {
                return Err(ProxyError::Local("cache digest mismatch".into()));
            }
            if let Some(failure) = policy(state, name, file, &sha256, date).await {
                return Ok(Err(failure));
            }
            Ok(Ok(Complete { size, hashes, head }))
        }
        Input::Upstream => {
            let repository = state
                .repository
                .as_ref()
                .ok_or_else(|| ProxyError::Local("missing repository".into()))?;
            let crate::config::repositories::RepositoryConfig::Proxy(upstream) =
                &repository.definition
            else {
                return Err(ProxyError::Local("missing upstream".into()));
            };
            let client = ProxyClient::for_raw_artifact(state);
            let deadline = match client {
                ProxyClient::Named { deadline, .. } => deadline,
                _ => None,
            };
            let auth = if same_origin(&file.url, &upstream.upstream) {
                upstream
                    .auth
                    .as_ref()
                    .map(|auth| UpstreamAuth::Basic(auth.expose()))
            } else {
                None
            };
            crate::registry::proxy_fetch_extract(
                client,
                &file.url,
                Duration::from_secs(state.config.pypi.proxy_timeout),
                auth,
                Some(("Accept-Encoding", "identity")),
                |response| async move {
                    if response.status() != StatusCode::OK {
                        return Err(ProxyError::Network(
                            "unexpected partial artifact response".into(),
                        ));
                    }
                    if response
                        .headers()
                        .get(header::CONTENT_ENCODING)
                        .is_some_and(|value| value != "identity")
                    {
                        return Err(ProxyError::Network(
                            "unexpected encoded artifact representation".into(),
                        ));
                    }
                    let length = crate::registry::content_length(response.headers());
                    if length
                        .zip(file.size)
                        .is_some_and(|(length, size)| length != size)
                    {
                        return Err(ProxyError::Network(
                            "artifact length differs from metadata".into(),
                        ));
                    }
                    let head = Head {
                        length,
                        validators: Validators {
                            etag: response
                                .headers()
                                .get(header::ETAG)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                            last_modified: response
                                .headers()
                                .get(header::LAST_MODIFIED)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                        },
                        cached: false,
                    };
                    verdict
                        .progress
                        .send_modify(|progress| progress.head = Some(head.clone()));
                    let stream = response.bytes_stream().map(|result| {
                        result.map_err(|error| ProxyError::Network(error.without_url().to_string()))
                    });
                    let (size, hashes) =
                        copy(stream, writer, file, length, &verdict.status).await?;
                    let sha256 = hashes
                        .get("sha256")
                        .ok_or_else(|| ProxyError::Local("missing transfer digest".into()))?;
                    if let Some(failure) = policy(state, name, file, sha256, date).await {
                        return Ok(Err(failure));
                    }
                    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                        return Err(ProxyError::Network("artifact operation timed out".into()));
                    }
                    Ok(Ok(Complete { size, hashes, head }))
                },
                &state.circuit_breaker,
                BreakerScope::upstream(RegistryType::PyPI, &upstream.upstream),
            )
            .await
        }
    }
}

async fn staging(path: &FsPath) -> Result<ActiveTemp, ProxyError> {
    let destination = path.with_extension(format!("cache-{}", uuid::Uuid::new_v4()));
    let owner = ActiveTemp::new(destination.clone(), TempFileGuard::new(destination.clone()));
    if tokio::fs::hard_link(path, &destination).await.is_err() {
        tokio::fs::copy(path, &destination)
            .await
            .map_err(|_| ProxyError::Local("cannot stage artifact cache write".into()))?;
    }
    Ok(owner)
}

async fn join(
    state: &AppState,
    name: &str,
    file: &model::Distribution,
    date: Option<i64>,
    input: Input,
) -> Result<Arc<Transfer>, Failure> {
    if state.cancel_token.is_cancelled() {
        return Err(Failure::status(StatusCode::SERVICE_UNAVAILABLE));
    }
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| Failure::status(StatusCode::INTERNAL_SERVER_ERROR))?;
    let binding = hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&(name, file, matches!(input, Input::Stored { .. })))
            .map_err(|_| Failure::status(StatusCode::INTERNAL_SERVER_ERROR))?,
    ));
    let directory = temp_dir(&state.config.storage.path);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|_| Failure::status(StatusCode::INTERNAL_SERVER_ERROR))?;
    let mut start_failure = None;
    let flight = repository
        .pypi_transfers
        .leased(&binding, "pypi", read_timeout(state), || async {
            let spool = match SpoolFile::create(&directory).await {
                Ok(spool) => spool,
                Err(_) => {
                    start_failure = Some(Failure::status(StatusCode::INTERNAL_SERVER_ERROR));
                    return None;
                }
            };
            let (status, status_rx) = watch::channel(SpoolStatus::default());
            let (progress, progress_rx) = watch::channel(Progress::default());
            let SpoolFile { path, file, guard } = spool;
            let owner = Arc::new(ActiveTemp::new(path.clone(), guard));
            let share = Arc::new(Transfer {
                path,
                _owner: owner,
                status: status_rx,
                progress: progress_rx,
            });
            Some((share, (file, Verdict { status, progress })))
        })
        .await
        .ok_or_else(|| start_failure.unwrap_or_else(|| Failure::status(StatusCode::BAD_GATEWAY)))?;
    let (share, writer, verdict, lease) = match flight {
        crate::proxy_coalesce::Flight::Follower(share) => return Ok(share),
        crate::proxy_coalesce::Flight::Leader {
            value,
            own: (writer, verdict),
            lease,
        } => (value, writer, verdict, Some(lease)),
        crate::proxy_coalesce::Flight::Alone {
            value,
            own: (writer, verdict),
        } => (value, writer, verdict, None),
    };
    let worker = state.clone();
    let name = name.to_owned();
    let file = file.clone();
    let shared = share.clone();
    let cancel = state.background_cancel.clone();
    state.background_tasks.spawn(async move {
        let _lease = lease;
        let mut writer = writer;
        let task = async {
            let result = match fill(&worker, &name, &file, date, input, &mut writer, &verdict).await {
                Ok(result) => result,
                Err(error) => Err(Failure::error(error).await),
            };
            verdict.progress.send_modify(|progress| progress.result = Some(result.clone()));
            match result {
                Ok(complete) => {
                    verdict.status.send_modify(|status| status.done = Some(Ok(())));
                    if !complete.head.cached {
                        let stored = async {
                            let stage = staging(&shared.path).await?;
                            cache::publish(&worker, &name, &file, complete.size, &complete.hashes, &complete.head.validators, &stage.path).await
                        }.await;
                        if let Err(error) = stored {
                            crate::metrics::CACHE_WRITE_ERRORS.with_label_values(&["pypi", "proxy_file_ready"]).inc();
                            tracing::warn!(error = ?error, package = %name, "PyPI transfer completed without durable cache readiness");
                        }
                    }
                }
                Err(_) => verdict.status.send_modify(|status| status.done = Some(Err("artifact transfer failed".into()))),
            }
        };
        tokio::select! { () = cancel.cancelled() => {}, () = task => {} }
        drop(writer);
    });
    Ok(share)
}

struct VerifiedSpool {
    inner: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
}
impl Stream for VerifiedSpool {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}
impl nora_registry::verified::stream_sealed::Sealed for VerifiedSpool {}
impl nora_registry::verified::VerifiedByteStream for VerifiedSpool {}

fn verified_spool(reader: tokio::fs::File, share: Arc<Transfer>, stall: Duration) -> VerifiedSpool {
    let stream = Box::pin(spool_follower(reader, share.status.clone(), 0, stall));
    let stream = futures::stream::unfold(
        (stream, sha2::Sha256::new(), 0u64, share, false),
        |(mut stream, mut hash, mut count, share, done)| async move {
            if done {
                return None;
            }
            match stream.next().await {
                Some(Ok(bytes)) => {
                    sha2::Digest::update(&mut hash, &bytes);
                    count += bytes.len() as u64;
                    let result = share.progress.borrow().result.clone();
                    if let Some(Ok(complete)) = result {
                        if count > complete.size
                            || (count == complete.size
                                && complete.hashes.get("sha256")
                                    != Some(&hex::encode(hash.clone().finalize())))
                        {
                            return Some((
                                Err(std::io::Error::other("artifact spool digest mismatch")),
                                (stream, hash, count, share, true),
                            ));
                        }
                    }
                    Some((Ok(bytes), (stream, hash, count, share, false)))
                }
                Some(Err(error)) => Some((Err(error), (stream, hash, count, share, true))),
                None => {
                    let valid = share
                        .progress
                        .borrow()
                        .result
                        .as_ref()
                        .is_some_and(|result| {
                            result.as_ref().is_ok_and(|complete| {
                                count == complete.size
                                    && complete.hashes.get("sha256")
                                        == Some(&hex::encode(hash.clone().finalize()))
                            })
                        });
                    if valid {
                        None
                    } else {
                        Some((
                            Err(std::io::Error::other("artifact transfer did not verify")),
                            (stream, hash, count, share, true),
                        ))
                    }
                }
            }
        },
    );
    VerifiedSpool {
        inner: Box::pin(stream),
    }
}

fn response_body(
    filename: &str,
    size: Option<u64>,
    validators: &Validators,
    body: Body,
) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(pypi_content_type(filename)),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response.headers_mut().insert(
        header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("bytes"),
    );
    if let Some(size) = size {
        if let Ok(value) = size.to_string().parse() {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    for (header, value) in [
        (header::ETAG, &validators.etag),
        (header::LAST_MODIFIED, &validators.last_modified),
    ] {
        if let Some(value) = value {
            if let Ok(value) = value.parse() {
                response.headers_mut().insert(header, value);
            }
        }
    }
    response
}

async fn follow(
    state: &AppState,
    name: &str,
    file: &model::Distribution,
    date: Option<i64>,
    input: Input,
) -> Response {
    let share = match join(state, name, file, date, input).await {
        Ok(share) => share,
        Err(failure) => return failure.into_response(),
    };
    let mut progress = share.progress.clone();
    let head = loop {
        let current = progress.borrow().clone();
        if let Some(Err(failure)) = current.result {
            return failure.into_response();
        }
        if let Some(head) = current.head {
            if !full_gate(state, name, &file.filename)
                && head.length != Some(0)
                && file.size != Some(0)
                || current.result.is_some()
            {
                break head;
            }
        }
        if progress.changed().await.is_err() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let reader = match tokio::fs::File::open(&share.path).await {
        Ok(reader) => reader,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    record_serve(state, name, &file.filename, head.cached);
    response_body(
        &file.filename,
        head.length.or(file.size),
        &head.validators,
        nora_registry::verified::stream_body(verified_spool(reader, share, read_timeout(state))),
    )
}

#[allow(clippy::too_many_arguments)]
async fn cached_response(
    state: &AppState,
    name: &str,
    file: &model::Distribution,
    date: Option<i64>,
    headers: &HeaderMap,
    size: u64,
    sha256: String,
    validators: Validators,
) -> Response {
    if full_gate(state, name, &file.filename) {
        return follow(
            state,
            name,
            file,
            date,
            Input::Stored {
                size,
                sha256,
                validators,
            },
        )
        .await;
    }
    if let Some(failure) = policy(state, name, file, &sha256, date).await {
        return failure.into_response();
    }
    let key = cache::body_key(name, &file.filename);
    if headers.contains_key(header::RANGE) {
        if let Some(response) = crate::registry::range::range_response(
            &state.storage,
            &[&key],
            headers,
            size,
            pypi_content_type(&file.filename),
            &[],
        )
        .await
        {
            if response.status() == StatusCode::PARTIAL_CONTENT {
                record_serve(state, name, &file.filename, true);
            }
            return response;
        }
    }
    let (length, pin, reader) = match state.storage.get_reader(&key).await {
        Ok(value) => value,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if length != size
        || pin.as_ref() != Some(&sha256)
        || size == 0 && sha256 != hex::encode(sha2::Sha256::digest([]))
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    record_serve(state, name, &file.filename, true);
    response_body(
        &file.filename,
        Some(size),
        &validators,
        nora_registry::verified::stream_body(nora_registry::verified::verify_while_streaming(
            reader,
            Some(sha256),
            key,
        )),
    )
}

pub(super) async fn download(
    state: &AppState,
    name: &str,
    filename: &str,
    headers: &HeaderMap,
    date: Option<i64>,
    view: named::View,
) -> Response {
    let Some(file) = view
        .snapshot
        .project
        .as_ref()
        .and_then(|project| project.files.iter().find(|file| file.filename == filename))
    else {
        return if view.offline {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response();
    };
    match cache::load(state, name, file, None).await {
        Ok(Some(ready)) => {
            let Some(sha256) = ready.hashes.get("sha256").cloned() else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            cached_response(
                state,
                name,
                file,
                date,
                headers,
                ready.size,
                sha256,
                ready.validators,
            )
            .await
        }
        Ok(None) if !view.offline => follow(state, name, file, date, Input::Upstream).await,
        Ok(None) => StatusCode::BAD_GATEWAY.into_response(),
        Err(error) => named::failure(error),
    }
}

pub(super) async fn local(
    state: &AppState,
    name: &str,
    filename: &str,
    headers: &HeaderMap,
    date: Option<i64>,
) -> Response {
    let key = cache::body_key(name, filename);
    let Some(meta) = state.storage.stat(&key).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(pin) = state.storage.pin(&key).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let file = model::Distribution {
        filename: filename.into(),
        url: String::new(),
        hashes: [("sha256".into(), pin.clone())].into(),
        requires_python: None,
        yanked: None,
        core_metadata: None,
        size: Some(meta.size),
        upload_time: None,
    };
    cached_response(
        state,
        name,
        &file,
        date,
        headers,
        meta.size,
        pin.clone(),
        Validators {
            etag: Some(format!("\"{pin}\"")),
            last_modified: None,
        },
    )
    .await
}
