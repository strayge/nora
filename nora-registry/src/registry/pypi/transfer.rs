// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Transfer completion and durable cache readiness are deliberately different events.

use super::cache::Artifact;
use super::{cache, model, named, *};
use crate::registry::spool::{spool_follower, SpoolFile, SpoolStatus};
use crate::registry::transport::ProxyClient;
use crate::registry::{ProxyError, TempFileGuard, Validators};
use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, Method};
use futures::{Stream, StreamExt};
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
    refusal: Option<Arc<crate::curation::CurationRefusal>>,
}
impl Failure {
    async fn response(response: Response) -> Self {
        let (parts, body) = response.into_parts();
        match axum::body::to_bytes(body, 64 * 1024).await {
            Ok(body) => Self {
                status: parts.status,
                headers: Arc::new(parts.headers),
                body,
                refusal: parts
                    .extensions
                    .get::<crate::curation::CurationRefusal>()
                    .cloned()
                    .map(Arc::new),
            },
            Err(_) => Self::status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: Arc::new(HeaderMap::new()),
            body: Bytes::new(),
            refusal: None,
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
        if let Some(refusal) = self.refusal {
            response.extensions_mut().insert(refusal.as_ref().clone());
        }
        response
    }
}

#[derive(Clone, Default)]
struct Head {
    status: StatusCode,
    length: Option<u64>,
    validators: Validators,
    cached: bool,
    content_range: Option<String>,
    no_body: bool,
}

#[derive(Clone)]
struct Request {
    method: Method,
    headers: HeaderMap,
}
impl Request {
    fn new(method: Method, headers: &HeaderMap) -> Self {
        let mut selected = HeaderMap::new();
        for name in [
            header::RANGE,
            header::IF_RANGE,
            header::IF_MATCH,
            header::IF_UNMODIFIED_SINCE,
            header::IF_NONE_MATCH,
            header::IF_MODIFIED_SINCE,
        ] {
            if method == Method::HEAD && (name == header::RANGE || name == header::IF_RANGE) {
                continue;
            }
            for value in headers.get_all(&name) {
                selected.append(name.clone(), value.clone());
            }
        }
        if selected
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                matches!(
                    crate::registry::range::parse_byte_range(value, u64::MAX),
                    crate::registry::range::ParsedRange::None
                )
            })
        {
            selected.remove(header::RANGE);
            selected.remove(header::IF_RANGE);
        }
        Self {
            method,
            headers: selected,
        }
    }
    fn origin_headers(&self, gated: bool, partial: bool) -> HeaderMap {
        let mut headers = if gated && self.method != Method::HEAD {
            HeaderMap::new()
        } else {
            self.headers.clone()
        };
        if !partial {
            headers.remove(header::RANGE);
            headers.remove(header::IF_RANGE);
        }
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        headers
    }
}

/// Opaque entity tags may contain commas, so a list cannot be split blindly.
fn tag_matches(value: &str, current: Option<&str>, weak: bool) -> bool {
    let mut rest = value.trim();
    if rest == "*" {
        return true;
    }
    let mut found = false;
    loop {
        let token = rest;
        let quoted = rest.strip_prefix("W/").unwrap_or(rest);
        let Some(quoted) = quoted.strip_prefix('"') else {
            return false;
        };
        let Some(end) = quoted.find('"') else {
            return false;
        };
        let length = token.len() - quoted.len() + end + 1;
        let tag = &token[..length];
        found |= current.is_some_and(|current| {
            if weak {
                tag.strip_prefix("W/").unwrap_or(tag)
                    == current.strip_prefix("W/").unwrap_or(current)
            } else {
                !tag.starts_with("W/") && !current.starts_with("W/") && tag == current
            }
        });
        rest = token[length..].trim_start();
        if rest.is_empty() {
            return found;
        }
        let Some(next) = rest.strip_prefix(',') else {
            return false;
        };
        rest = next.trim_start();
    }
}
fn http_date(value: Option<&str>) -> Option<std::time::SystemTime> {
    value.and_then(|value| httpdate::parse_http_date(value).ok())
}
fn tag_header(
    headers: &HeaderMap,
    name: HeaderName,
    current: Option<&str>,
    weak: bool,
) -> Option<bool> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    let values = std::iter::once(first)
        .chain(values)
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>();
    Some(values.is_ok_and(|values| tag_matches(&values.join(","), current, weak)))
}

fn conditional(headers: &HeaderMap, validators: &Validators) -> Option<StatusCode> {
    let get = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(matched) = tag_header(headers, header::IF_MATCH, validators.etag.as_deref(), false)
    {
        if !matched {
            return Some(StatusCode::PRECONDITION_FAILED);
        }
    } else if http_date(validators.last_modified.as_deref())
        .zip(http_date(get(header::IF_UNMODIFIED_SINCE)))
        .is_some_and(|(modified, limit)| modified > limit)
    {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    if let Some(matched) = tag_header(
        headers,
        header::IF_NONE_MATCH,
        validators.etag.as_deref(),
        true,
    ) {
        if matched {
            return Some(StatusCode::NOT_MODIFIED);
        }
    } else if http_date(validators.last_modified.as_deref())
        .zip(http_date(get(header::IF_MODIFIED_SINCE)))
        .is_some_and(|(modified, limit)| modified <= limit)
    {
        return Some(StatusCode::NOT_MODIFIED);
    }
    None
}
fn range_allowed(headers: &HeaderMap, validators: &Validators) -> bool {
    let Some(value) = headers.get(header::IF_RANGE) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    if value.starts_with('"') || value.starts_with("W/") {
        return !value.starts_with("W/")
            && validators
                .etag
                .as_deref()
                .is_some_and(|etag| !etag.starts_with("W/") && etag == value);
    }
    http_date(validators.last_modified.as_deref())
        .zip(http_date(Some(value)))
        .is_some_and(|(modified, limit)| modified <= limit)
}
fn partial_allowed(state: &AppState, name: &str, file: &Artifact) -> bool {
    matches!(
        quarantine(state).0,
        crate::digest_quarantine::QuarantineMode::Off
    ) && !crate::curation::integrity_applies(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &file.coordinate().filename).as_deref(),
    )
}

#[derive(Clone)]
struct Complete {
    size: u64,
    hashes: BTreeMap<String, String>,
    head: Head,
    cacheable: bool,
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
    state.log_audit(AuditEntry::new(
        if cached { "cache_hit" } else { "proxy_fetch" },
        "proxy",
        "",
        "pypi",
        "",
    ));
}

fn full_gate(state: &AppState, name: &str, file: &Artifact) -> bool {
    matches!(
        quarantine(state).0,
        crate::digest_quarantine::QuarantineMode::Enforce
    ) || crate::curation::integrity_applies(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &file.coordinate().filename).as_deref(),
    )
}

pub(super) async fn parent_policy(
    state: &AppState,
    name: &str,
    file: &Artifact,
) -> Option<Response> {
    let parent = file.parent.as_ref()?;
    if !crate::curation::integrity_applies(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &parent.filename).as_deref(),
    ) {
        return None;
    }
    let ready = match cache::load_artifact(
        state,
        name,
        &Artifact::distribution(parent.clone()),
        None,
        false,
    )
    .await
    {
        Ok(Some(ready)) => ready,
        _ => {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    "Core Metadata requires a verified parent artifact",
                )
                    .into_response(),
            )
        }
    };
    let Some(sha256) = ready.hashes.get("sha256") else {
        return Some(StatusCode::FORBIDDEN.into_response());
    };
    crate::curation::verify_integrity_by_hash(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &parent.filename).as_deref(),
        &format!("sha256:{sha256}"),
    )
}

pub(super) async fn metadata_available(
    state: &AppState,
    name: &str,
    parent: &model::Distribution,
    offline: bool,
) -> bool {
    let Some(file) = Artifact::sidecar(parent) else {
        return false;
    };
    if parent_policy(state, name, &file).await.is_some() {
        return false;
    }
    let (mode, seconds) = quarantine(state);
    let ready = if offline || matches!(mode, crate::digest_quarantine::QuarantineMode::Enforce) {
        cache::load_artifact(state, name, &file, None, true)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    if offline && ready.is_none() {
        return false;
    }
    if !matches!(mode, crate::digest_quarantine::QuarantineMode::Enforce) {
        return true;
    }
    let digest = ready
        .as_ref()
        .and_then(|ready| ready.hashes.get("sha256"))
        .or_else(|| file.hashes.get("sha256"));
    if digest.is_some_and(|digest| {
        matches!(
            state
                .digest_store
                .check("pypi", &format!("sha256:{digest}"), seconds),
            crate::digest_quarantine::QuarantineStatus::Mature
        )
    }) {
        return true;
    }
    seconds <= 0
        || state.config.server.trust_upstream_dates
            && parent
                .upload_time
                .as_ref()
                .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
                .is_some_and(|date| {
                    date.timestamp() <= chrono::Utc::now().timestamp().saturating_sub(seconds)
                })
}

async fn policy(
    state: &AppState,
    name: &str,
    file: &Artifact,
    sha256: &str,
    date: Option<i64>,
) -> Option<Failure> {
    let digest = format!("sha256:{sha256}");
    if file.parent.is_some() {
        if let Some(response) = parent_policy(state, name, file).await {
            return Some(Failure::response(response).await);
        }
    } else if let Some(response) = crate::curation::verify_integrity_by_hash(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
        crate::curation::parse_pypi_version(name, &file.filename).as_deref(),
        &digest,
    ) {
        return Some(Failure::response(response).await);
    }
    let date = if file.parent.is_some() && !state.config.server.trust_upstream_dates {
        None
    } else {
        date
    };
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

fn source_partial(
    headers: &HeaderMap,
    request: &HeaderMap,
    known: Option<u64>,
) -> Result<(u64, String), ProxyError> {
    let invalid = || ProxyError::Network("invalid upstream Content-Range".into());
    let value = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(invalid)?;
    let (range, total) = value
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(invalid)?;
    let (start, end) = range.split_once('-').ok_or_else(invalid)?;
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let end: u64 = end.parse().map_err(|_| invalid())?;
    let length = end
        .checked_sub(start)
        .and_then(|length| length.checked_add(1))
        .ok_or_else(invalid)?;
    let total: Option<u64> = if total == "*" {
        None
    } else {
        Some(total.parse().map_err(|_| invalid())?)
    };
    if total.is_some_and(|size| end >= size)
        || known.is_some_and(|size| end >= size || total.is_some_and(|total| total != size))
    {
        return Err(invalid());
    }
    let requested = request
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(invalid)?;
    if let Some(total) = known.or(total) {
        if crate::registry::range::parse_byte_range(requested, total)
            != crate::registry::range::ParsedRange::Satisfiable(start, end)
        {
            return Err(invalid());
        }
    } else {
        let (first, last) = requested
            .strip_prefix("bytes=")
            .and_then(|value| value.trim().split_once('-'))
            .ok_or_else(invalid)?;
        let requested_start: u64 = first.trim().parse().map_err(|_| invalid())?;
        let requested_end = if last.trim().is_empty() {
            None
        } else {
            Some(last.trim().parse::<u64>().map_err(|_| invalid())?)
        };
        if start != requested_start || requested_end.is_some_and(|requested| end > requested) {
            return Err(invalid());
        }
    }
    Ok((length, value.into()))
}

#[allow(clippy::too_many_arguments)]
async fn fill(
    state: &AppState,
    name: &str,
    file: &Artifact,
    date: Option<i64>,
    input: Input,
    request: &Request,
    writer: &mut tokio::fs::File,
    verdict: &Verdict,
) -> Result<Result<Complete, Failure>, ProxyError> {
    let input = if matches!(input, Input::Upstream) {
        match cache::load_artifact(state, name, file, None, request.method != Method::HEAD).await? {
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
            let status = conditional(&request.headers, &validators).unwrap_or(StatusCode::OK);
            let head = Head {
                status,
                length: if status == StatusCode::PRECONDITION_FAILED {
                    None
                } else {
                    Some(size)
                },
                validators,
                cached: true,
                content_range: None,
                no_body: request.method == Method::HEAD || status != StatusCode::OK,
            };
            if head.no_body {
                if request.method != Method::HEAD {
                    if let Some(failure) = policy(state, name, file, &sha256, date).await {
                        return Ok(Err(failure));
                    }
                }
                verdict
                    .progress
                    .send_modify(|progress| progress.head = Some(head.clone()));
                return Ok(Ok(Complete {
                    size: 0,
                    hashes: BTreeMap::new(),
                    head,
                    cacheable: false,
                }));
            }
            let (length, pin, reader) = state
                .storage
                .get_reader(&cache::body_key(name, &file.filename))
                .await
                .map_err(|_| ProxyError::Local("cached artifact unavailable".into()))?;
            if length != size || pin.as_ref() != Some(&sha256) {
                return Err(ProxyError::Local("cached artifact changed".into()));
            }
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
            Ok(Ok(Complete {
                size,
                hashes,
                head,
                cacheable: false,
            }))
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
            let client = if request.method == Method::HEAD {
                ProxyClient::for_raw_metadata(state)
            } else {
                ProxyClient::for_raw_artifact(state)
            };
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
            let origin_headers = request.origin_headers(
                full_gate(state, name, file),
                partial_allowed(state, name, file),
            );
            let mut upstream_request = client
                .request(request.method.clone(), &file.url)
                .timeout(Duration::from_secs(state.config.pypi.proxy_timeout))
                .headers(origin_headers.clone());
            if let Some(auth) = crate::registry::upstream_auth_header(auth)? {
                upstream_request = upstream_request.header(header::AUTHORIZATION, auth);
            }
            crate::registry::proxy_fetch_request(
                client,
                &file.url,
                upstream_request,
                true,
                |response| async move {
                    let status = response.status();
                    // Origin diagnostics can echo signed targets; only safe protocol headers cross this boundary.
                    let no_body = request.method == Method::HEAD
                        || matches!(status.as_u16(), 304 | 412 | 416);
                    if !matches!(status.as_u16(), 200 | 206 | 304 | 412 | 416)
                        || status == StatusCode::PARTIAL_CONTENT && request.method == Method::HEAD
                        || status == StatusCode::NOT_MODIFIED
                            && !origin_headers.contains_key(header::IF_NONE_MATCH)
                            && !origin_headers.contains_key(header::IF_MODIFIED_SINCE)
                        || status == StatusCode::PRECONDITION_FAILED
                            && !origin_headers.contains_key(header::IF_MATCH)
                            && !origin_headers.contains_key(header::IF_UNMODIFIED_SINCE)
                        || status == StatusCode::RANGE_NOT_SATISFIABLE
                            && !origin_headers.contains_key(header::RANGE)
                    {
                        return Err(ProxyError::Network(
                            "unexpected artifact response status".into(),
                        ));
                    }
                    if !matches!(status.as_u16(), 412 | 416)
                        && response
                            .headers()
                            .get(header::CONTENT_ENCODING)
                            .is_some_and(|value| {
                                !value
                                    .to_str()
                                    .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
                            })
                    {
                        return Err(ProxyError::Network(
                            "unexpected encoded artifact representation".into(),
                        ));
                    }
                    if status == StatusCode::OK
                        && response.headers().contains_key(header::CONTENT_RANGE)
                    {
                        return Err(ProxyError::Network(
                            "partial Content-Range on a full artifact response".into(),
                        ));
                    }
                    let mut length = crate::registry::content_length(response.headers());
                    let mut content_range = None;
                    if status == StatusCode::PARTIAL_CONTENT {
                        let (partial, range) =
                            source_partial(response.headers(), &origin_headers, file.size)?;
                        if length.is_some_and(|length| length != partial) {
                            return Err(ProxyError::Network(
                                "partial length differs from Content-Range".into(),
                            ));
                        }
                        length = Some(partial);
                        content_range = Some(range);
                    } else if status == StatusCode::RANGE_NOT_SATISFIABLE {
                        let range = response
                            .headers()
                            .get(header::CONTENT_RANGE)
                            .and_then(|value| value.to_str().ok())
                            .ok_or_else(|| {
                                ProxyError::Network("missing unsatisfied Content-Range".into())
                            })?;
                        let total: u64 = range
                            .strip_prefix("bytes */")
                            .and_then(|total| total.parse().ok())
                            .ok_or_else(|| {
                                ProxyError::Network("invalid unsatisfied Content-Range".into())
                            })?;
                        if file.size.is_some_and(|size| size != total) {
                            return Err(ProxyError::Network(
                                "unsatisfied range size differs from metadata".into(),
                            ));
                        }
                        content_range = Some(range.into());
                        length = None;
                    } else if status == StatusCode::PRECONDITION_FAILED {
                        length = None;
                    }
                    if status == StatusCode::OK
                        && length
                            .zip(file.size)
                            .is_some_and(|(length, size)| length != size)
                    {
                        return Err(ProxyError::Network(
                            "artifact length differs from metadata".into(),
                        ));
                    }
                    let mut head = Head {
                        status,
                        length,
                        content_range,
                        no_body,
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
                    if no_body {
                        return Ok(Ok(Complete {
                            size: 0,
                            hashes: BTreeMap::new(),
                            head,
                            cacheable: false,
                        }));
                    }
                    let mut representation = file.file.clone();
                    if status == StatusCode::PARTIAL_CONTENT {
                        representation.hashes.clear();
                        representation.size = length;
                    }
                    let stream = response.bytes_stream().map(|result| {
                        result.map_err(|error| ProxyError::Network(error.without_url().to_string()))
                    });
                    let (size, hashes) =
                        copy(stream, writer, &representation, length, &verdict.status).await?;
                    let sha256 = hashes
                        .get("sha256")
                        .ok_or_else(|| ProxyError::Local("missing transfer digest".into()))?;
                    if status == StatusCode::OK {
                        if let Some(failure) = policy(state, name, file, sha256, date).await {
                            return Ok(Err(failure));
                        }
                        if full_gate(state, name, file) {
                            if let Some(status) = conditional(&request.headers, &head.validators) {
                                head.status = status;
                                head.no_body = true;
                                if status == StatusCode::PRECONDITION_FAILED {
                                    head.length = None;
                                }
                            }
                        }
                    }
                    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                        return Err(ProxyError::Network("artifact operation timed out".into()));
                    }
                    Ok(Ok(Complete {
                        size,
                        hashes,
                        head,
                        cacheable: status == StatusCode::OK,
                    }))
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
    file: &Artifact,
    date: Option<i64>,
    request: &Request,
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
        serde_json::to_vec(&(
            name,
            file,
            matches!(input, Input::Stored { .. }),
            request.method.as_str(),
            request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes()))
                .collect::<Vec<_>>(),
        ))
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
    let mut worker = state.clone();
    worker.metadata_deadline = None;
    if request.method == Method::HEAD {
        crate::registry::transport::begin_metadata(&mut worker);
    }
    let request = request.clone();
    let name = name.to_owned();
    let file = file.clone();
    let shared = share.clone();
    let cancel = state.background_cancel.clone();
    state.background_tasks.spawn(async move {
        let _lease = lease;
        let mut writer = writer;
        let task = async {
            let result = match fill(&worker, &name, &file, date, input, &request, &mut writer, &verdict).await {
                Ok(result) => result,
                Err(error) => Err(Failure::error(error).await),
            };
            verdict.progress.send_modify(|progress| {
                if let Ok(complete) = &result { progress.head = Some(complete.head.clone()); }
                progress.result = Some(result.clone());
            });
            match result {
                Ok(complete) => {
                    verdict.status.send_modify(|status| status.done = Some(Ok(())));
                    if complete.cacheable && !complete.head.cached && crate::registry::transport::policy(&worker).is_none_or(|policy| policy.save_files) {
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

fn response_body(filename: &str, head: &Head, body: Body) -> Response {
    let mut response = body.into_response();
    *response.status_mut() = head.status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(pypi_content_type(filename)),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(
            if head.status.is_success() || head.status == StatusCode::NOT_MODIFIED {
                "public, max-age=31536000, immutable"
            } else {
                "no-store"
            },
        ),
    );
    response.headers_mut().insert(
        header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("bytes"),
    );
    if let Some(range) = &head.content_range {
        if let Ok(value) = range.parse() {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    if let Some(size) = head.length {
        if let Ok(value) = size.to_string().parse() {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    for (header, value) in [
        (header::ETAG, &head.validators.etag),
        (header::LAST_MODIFIED, &head.validators.last_modified),
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
    file: &Artifact,
    date: Option<i64>,
    request: &Request,
    input: Input,
) -> Response {
    let share = match join(state, name, file, date, request, input).await {
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
            if head.no_body
                || !full_gate(state, name, file) && head.length != Some(0) && file.size != Some(0)
                || current.result.is_some()
            {
                break head;
            }
        }
        if progress.changed().await.is_err() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if head.no_body {
        return response_body(&file.filename, &head, Body::empty());
    }
    let reader = match tokio::fs::File::open(&share.path).await {
        Ok(reader) => reader,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    record_serve(state, name, &file.filename, head.cached);
    response_body(
        &file.filename,
        &Head {
            length: head.length.or(file.size),
            ..head
        },
        nora_registry::verified::stream_body(verified_spool(reader, share, read_timeout(state))),
    )
}

#[allow(clippy::too_many_arguments)]
async fn cached_response(
    state: &AppState,
    name: &str,
    file: &Artifact,
    date: Option<i64>,
    method: Method,
    headers: &HeaderMap,
    size: u64,
    sha256: String,
    validators: Validators,
) -> Response {
    let request = Request::new(method, headers);
    let status = conditional(&request.headers, &validators).unwrap_or(StatusCode::OK);
    let head = Head {
        status,
        length: if status == StatusCode::PRECONDITION_FAILED {
            None
        } else {
            Some(size)
        },
        validators: validators.clone(),
        cached: true,
        no_body: request.method == Method::HEAD || status != StatusCode::OK,
        content_range: None,
    };
    if head.no_body {
        if request.method != Method::HEAD {
            if let Some(failure) = policy(state, name, file, &sha256, date).await {
                return failure.into_response();
            }
        }
        return response_body(&file.filename, &head, Body::empty());
    }
    if full_gate(state, name, file) {
        return follow(
            state,
            name,
            file,
            date,
            &request,
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
    if partial_allowed(state, name, file)
        && range_allowed(&request.headers, &validators)
        && request.headers.contains_key(header::RANGE)
    {
        let mut extra = vec![(
            header::CACHE_CONTROL,
            "public, max-age=31536000, immutable".into(),
        )];
        if let Some(etag) = &validators.etag {
            extra.push((header::ETAG, etag.clone()));
        }
        if let Some(modified) = &validators.last_modified {
            extra.push((header::LAST_MODIFIED, modified.clone()));
        }
        if let Some(response) = crate::registry::range::range_response_of(
            &state.storage,
            &[&key],
            &request.headers,
            size,
            Some(&sha256),
            pypi_content_type(&file.filename),
            &extra,
        )
        .await
        {
            if response.status() == StatusCode::PARTIAL_CONTENT {
                record_serve(state, name, &file.filename, true);
            } else if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                return response_body(
                    &file.filename,
                    &Head {
                        status: StatusCode::RANGE_NOT_SATISFIABLE,
                        length: None,
                        content_range: Some(format!("bytes */{size}")),
                        no_body: true,
                        ..head
                    },
                    Body::empty(),
                );
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
        &head,
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
    method: Method,
    headers: &HeaderMap,
    date: Option<i64>,
    view: named::View,
) -> Response {
    let Some(file) = view
        .snapshot
        .project
        .as_ref()
        .and_then(|project| Artifact::resolve(project, filename))
    else {
        return if view.offline {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response();
    };
    if method != Method::HEAD {
        if let Some(response) = parent_policy(state, name, &file).await {
            return response;
        }
    }
    match cache::load_artifact(state, name, &file, None, method != Method::HEAD).await {
        Ok(Some(ready)) => {
            let Some(sha256) = ready.hashes.get("sha256").cloned() else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            cached_response(
                state,
                name,
                &file,
                date,
                method,
                headers,
                ready.size,
                sha256,
                ready.validators,
            )
            .await
        }
        Ok(None) if !view.offline => {
            follow(
                state,
                name,
                &file,
                date,
                &Request::new(method, headers),
                Input::Upstream,
            )
            .await
        }
        Ok(None) => StatusCode::BAD_GATEWAY.into_response(),
        Err(error) => named::failure(error),
    }
}

pub(super) async fn local(
    state: &AppState,
    name: &str,
    filename: &str,
    method: Method,
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
    let file = Artifact::distribution(model::Distribution {
        filename: filename.into(),
        url: String::new(),
        hashes: [("sha256".into(), pin.clone())].into(),
        requires_python: None,
        yanked: None,
        core_metadata: None,
        size: Some(meta.size),
        upload_time: None,
    });
    cached_response(
        state,
        name,
        &file,
        date,
        method,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_entity_tag_lists_preserve_opaque_commas_weakness_and_repeated_fields() {
        assert!(tag_matches(
            "\"other\", W/\"has,a,comma\"",
            Some("\"has,a,comma\""),
            true
        ));
        assert!(!tag_matches("W/\"tag\"", Some("\"tag\""), false));
        assert!(!tag_matches("\"tag\", broken", Some("\"tag\""), true));
        let mut headers = HeaderMap::new();
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        headers.append(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"has,a,comma\""),
        );
        let validators = Validators {
            etag: Some("\"has,a,comma\"".into()),
            last_modified: None,
        };
        assert_eq!(
            conditional(&headers, &validators),
            Some(StatusCode::NOT_MODIFIED)
        );
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"wrong\""));
        assert_eq!(
            conditional(&headers, &validators),
            Some(StatusCode::PRECONDITION_FAILED)
        );
    }

    #[tokio::test]
    async fn only_trusted_upstream_dates_may_seed_a_new_sidecar_digest_not_the_parent_first_seen_clock(
    ) {
        let _lock = crate::test_env::env_lock();
        for trusted in [false, true] {
            let context = crate::test_helpers::create_test_context_with_config(|config| {
                config.server.trust_upstream_dates = trusted;
                config.curation.pypi.quarantine =
                    Some(crate::digest_quarantine::QuarantineMode::Enforce);
                config.curation.pypi.quarantine_ttl = Some("1h".into());
            });
            let parent = model::Distribution {
                filename: "pkg-1.0-py3-none-any.whl".into(),
                url: "https://source.example/pkg.whl".into(),
                hashes: BTreeMap::new(),
                requires_python: None,
                yanked: None,
                core_metadata: Some(model::CoreMetadata::Flag(true)),
                size: None,
                upload_time: None,
            };
            let file = Artifact::sidecar(&parent).unwrap();
            let blocked = policy(
                &context.state,
                "pkg",
                &file,
                &"00".repeat(32),
                Some(chrono::Utc::now().timestamp() - 7200),
            )
            .await;
            assert_eq!(blocked.is_some(), !trusted);
        }
    }
}
