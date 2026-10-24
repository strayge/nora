// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Source snapshots keep absence, constraints, and status distinct from availability.

use super::model::{self, Project, Status};
use super::*;
use crate::registry::transport::ProxyClient;
use crate::registry::{ProxyError, Revalidation, Validators};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) const SNAPSHOTS: &str = ".nora-pypi-index/";
const FIRST_SEEN: &str = ".nora-pypi-first-seen/";
const RECOVERY: &str = "\0source-recovery";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(super) schema: u32,
    #[serde(default)]
    pub(super) name: String,
    pub(super) source_id: String,
    pub(super) fetched_at: u64,
    pub(super) validators: Validators,
    pub(super) page_url: String,
    pub(super) content_type: String,
    // Missing metadata is malformed, not a confirmed cached absence.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) project: Option<Arc<Project>>,
}

#[derive(Clone)]
pub(super) struct View {
    pub snapshot: Arc<Snapshot>,
    pub offline: bool,
}

/// Late timeouts cannot fall back past a quarantine observed by this refresh.
#[derive(Clone, Default)]
pub(crate) struct Progress {
    previous: Option<Arc<Snapshot>>,
    quarantine: Option<Arc<Snapshot>>,
    result: Option<Result<View, ProxyError>>,
}

type RefreshRx = tokio::sync::watch::Receiver<Progress>;

#[derive(Serialize, Deserialize)]
struct SeenFile {
    fingerprint: String,
    first_seen: i64,
}

#[derive(Serialize, Deserialize)]
struct FirstSeen {
    source_id: String,
    files: BTreeMap<String, SeenFile>,
}

pub(super) fn is_proxy(state: &AppState) -> bool {
    state.repository.as_ref().is_some_and(|repository| {
        matches!(
            repository.definition,
            crate::config::repositories::RepositoryConfig::Proxy(_)
        )
    })
}

fn unavailable(message: &'static str) -> ProxyError {
    ProxyError::Network(message.into())
}

async fn budget<T>(
    state: &AppState,
    future: impl std::future::Future<Output = Result<T, ProxyError>>,
) -> Result<T, ProxyError> {
    if let Some(deadline) = state.metadata_deadline {
        if tokio::time::Instant::now() >= deadline {
            return Err(unavailable("project operation timed out"));
        }
        let result = tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| unavailable("project operation timed out"))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(unavailable("project operation timed out"));
        }
        result
    } else {
        future.await
    }
}

async fn verified(state: &AppState, key: &str) -> Result<Option<Bytes>, ProxyError> {
    budget(state, async {
        match state.storage.get_verified(key).await {
            Ok(nora_registry::verified::GateOutcome::Verified(blob)) => {
                Ok(Some(nora_registry::verified::verified_body(blob)))
            }
            Ok(_) => Err(unavailable("untrusted project cache record")),
            Err(crate::storage::StorageError::NotFound) => Ok(None),
            Err(_) => Err(unavailable("project cache record unavailable")),
        }
    })
    .await
}

fn remembered(state: &AppState, name: &str) -> Option<Arc<Snapshot>> {
    state
        .repository
        .as_ref()?
        .pypi_snapshots
        .read()
        .get(name)
        .cloned()
}

pub(super) async fn stored(
    state: &AppState,
    name: &str,
) -> Result<Option<Arc<Snapshot>>, ProxyError> {
    if let Some(snapshot) = remembered(state, name) {
        return Ok(Some(snapshot));
    }
    let Some(repository) = state.repository.as_ref() else {
        return Ok(None);
    };
    let Some(bytes) = verified(state, &format!("{SNAPSHOTS}{name}.json")).await? else {
        return Ok(None);
    };
    let mut snapshot: Snapshot = budget(state, async move {
        tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes))
            .await
            .map_err(|_| unavailable("project cache parser failed"))?
            .map_err(|_| unavailable("invalid project cache record"))
    })
    .await?;
    if snapshot.schema == 1 {
        let project = snapshot
            .project
            .as_ref()
            .ok_or_else(|| unavailable("invalid legacy project snapshot"))?;
        snapshot.name.clone_from(&project.name);
        snapshot.schema = 2;
    }
    if snapshot.schema != 2
        || snapshot.name != name
        || snapshot.source_id != repository.source_id
        || snapshot
            .project
            .as_ref()
            .is_some_and(|project| project.name != name)
    {
        return Err(unavailable("project cache source mismatch"));
    }
    Ok(Some(Arc::new(snapshot)))
}

async fn record_seen(state: &AppState, project: &Project) -> Result<(), ProxyError> {
    if state.config.server.trust_upstream_dates || !state.curation().curation_engine.is_active() {
        return Ok(());
    }
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let key = format!("{FIRST_SEEN}{}.json", project.name);
    let mut ledger: FirstSeen = match verified(state, &key).await? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| unavailable("invalid project first-seen record"))?,
        None => FirstSeen {
            source_id: repository.source_id.clone(),
            files: BTreeMap::new(),
        },
    };
    if ledger.source_id != repository.source_id {
        return Err(unavailable("project first-seen source mismatch"));
    }
    let now = crate::cache_ttl::now_unix() as i64;
    let mut changed = false;
    for file in &project.files {
        let hashes =
            serde_json::to_vec(&file.hashes).map_err(|_| unavailable("invalid file hashes"))?;
        let fingerprint = hex::encode(sha2::Sha256::digest(&hashes));
        if ledger
            .files
            .get(&file.filename)
            .is_none_or(|entry| entry.fingerprint != fingerprint)
        {
            ledger.files.insert(
                file.filename.clone(),
                SeenFile {
                    fingerprint,
                    first_seen: now,
                },
            );
            changed = true;
        }
    }
    if changed {
        let bytes = serde_json::to_vec(&ledger)
            .map_err(|_| unavailable("cannot encode first-seen record"))?;
        state
            .storage
            .put(&key, &bytes)
            .await
            .map_err(|_| unavailable("cannot persist first-seen record"))?;
    }
    Ok(())
}

fn upstream_url(state: &AppState, name: &str) -> Result<reqwest::Url, ProxyError> {
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let crate::config::repositories::RepositoryConfig::Proxy(policy) = &repository.definition
    else {
        return Err(unavailable("missing repository upstream"));
    };
    let mut url = reqwest::Url::parse(&policy.upstream)
        .map_err(|_| unavailable("invalid repository upstream"))?;
    url.path_segments_mut()
        .map_err(|_| unavailable("invalid repository upstream path"))?
        .pop_if_empty()
        .push(name)
        .push("");
    Ok(url)
}

fn page_location(mut url: reqwest::Url) -> String {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    url.to_string()
}

async fn request_project(
    state: &AppState,
    name: &str,
    accept: &'static str,
    validators: &Validators,
    previous: Option<&Snapshot>,
) -> Result<Revalidation<Snapshot>, ProxyError> {
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let crate::config::repositories::RepositoryConfig::Proxy(policy) = &repository.definition
    else {
        return Err(unavailable("missing repository upstream"));
    };
    let url = upstream_url(state, name)?;
    crate::registry::proxy_fetch_conditional_extract(
        ProxyClient::for_state(state),
        url.as_str(),
        Duration::from_secs(state.config.pypi.proxy_timeout),
        policy
            .auth
            .as_ref()
            .map(|auth| UpstreamAuth::Basic(auth.expose())),
        validators,
        Some(("Accept", accept)),
        true,
        |response| async move {
            if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                let mut snapshot = previous
                    .filter(|snapshot| snapshot.project.is_some())
                    .ok_or_else(|| unavailable("unexpected project NotModified response"))?
                    .clone();
                snapshot.fetched_at = crate::cache_ttl::now_unix();
                return Ok(snapshot);
            }
            let page_url = response.url().clone();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| ProxyError::Network(error.without_url().to_string()))?;
            let json = content_type.contains("json")
                || bytes
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace())
                    == Some(b'{');
            let parse_name = name.to_owned();
            let parse_url = page_url.clone();
            let project = tokio::task::spawn_blocking(move || {
                if json {
                    model::parse_json(&bytes, &parse_name, &parse_url)
                } else {
                    model::parse_html(&bytes, &parse_name, &parse_url)
                }
            })
            .await
            .map_err(|_| unavailable("project metadata parser failed"))?
            .map_err(unavailable)?;
            if state
                .metadata_deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            {
                return Err(unavailable("project operation timed out"));
            }
            Ok(Snapshot {
                schema: 2,
                name: name.into(),
                source_id: repository.source_id.clone(),
                fetched_at: crate::cache_ttl::now_unix(),
                validators: Validators::default(),
                page_url: page_location(page_url),
                content_type,
                project: Some(Arc::new(project)),
            })
        },
        &state.circuit_breaker,
        BreakerScope::upstream(RegistryType::PyPI, &policy.upstream),
    )
    .await
}

async fn refresh(
    state: &AppState,
    name: &str,
    previous: Option<Arc<Snapshot>>,
    recovery: bool,
    progress: &tokio::sync::watch::Sender<Progress>,
) -> Result<Arc<Snapshot>, ProxyError> {
    let key = format!("{SNAPSHOTS}{name}.json");
    let lock = state.publish_lock(&key);
    let _guard = lock.lock().await;
    let previous = stored(state, name).await?.or(previous);
    let validators = previous
        .as_ref()
        .filter(|snapshot| snapshot.project.is_some())
        .map(|snapshot| snapshot.validators.clone())
        .unwrap_or_default();
    let fetched = match request_project(
        state,
        name,
        "application/vnd.pypi.simple.v1+json, text/html;q=0.1",
        &validators,
        previous.as_deref(),
    )
    .await
    {
        Err(ProxyError::Upstream(406 | 415)) => {
            request_project(state, name, "text/html", &validators, previous.as_deref()).await
        }
        result => result,
    };
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let snapshot = match fetched {
        Ok(Revalidation::Modified {
            mut body,
            validators,
        }) => {
            body.validators = Validators {
                etag: validators.etag.or(body.validators.etag),
                last_modified: validators.last_modified.or(body.validators.last_modified),
            };
            body
        }
        Ok(Revalidation::NotModified) => {
            let mut snapshot = previous
                .filter(|snapshot| snapshot.project.is_some())
                .ok_or_else(|| unavailable("unexpected project NotModified response"))?
                .as_ref()
                .clone();
            snapshot.fetched_at = crate::cache_ttl::now_unix();
            snapshot
        }
        Err(ProxyError::NotFound) => {
            if recovery && !state.circuit_breaker.is_enabled() {
                state.circuit_breaker.record_success(
                    &repository.breaker_key(),
                    crate::circuit_breaker::ProbeToken::BACKGROUND,
                );
            }
            if let Some(previous) = previous.filter(|snapshot| quarantined(snapshot)) {
                let mut snapshot = previous.as_ref().clone();
                snapshot.fetched_at = crate::cache_ttl::now_unix();
                snapshot.validators = Validators::default();
                snapshot
            } else {
                Snapshot {
                    schema: 2,
                    name: name.into(),
                    source_id: repository.source_id.clone(),
                    fetched_at: crate::cache_ttl::now_unix(),
                    validators: Validators::default(),
                    page_url: page_location(upstream_url(state, name)?),
                    content_type: String::new(),
                    project: None,
                }
            }
        }
        Err(error) => return Err(error),
    };
    let snapshot = Arc::new(snapshot);
    if quarantined(&snapshot) {
        repository
            .pypi_snapshots
            .write()
            .insert(name.into(), snapshot.clone());
        progress.send_modify(|progress| progress.quarantine = Some(snapshot.clone()));
    }
    if let Some(project) = &snapshot.project {
        record_seen(state, project).await?;
    }
    let bytes =
        serde_json::to_vec(&snapshot).map_err(|_| unavailable("cannot encode project snapshot"))?;
    state
        .storage
        .put(&key, &bytes)
        .await
        .map_err(|_| unavailable("cannot persist project snapshot"))?;
    repository.pypi_snapshots.write().remove(name);
    Ok(snapshot)
}

fn present(snapshot: Arc<Snapshot>, offline: bool) -> Result<View, ProxyError> {
    if snapshot.project.is_none() {
        return Err(ProxyError::NotFound);
    }
    Ok(View { snapshot, offline })
}

async fn resolve(
    state: &AppState,
    name: &str,
    progress: &tokio::sync::watch::Sender<Progress>,
) -> Result<View, ProxyError> {
    let previous = stored(state, name).await?;
    progress.send_modify(|progress| progress.previous.clone_from(&previous));
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let crate::config::repositories::RepositoryConfig::Proxy(policy) = &repository.definition
    else {
        return Err(unavailable("missing repository upstream"));
    };
    let fresh = remembered(state, name).is_none()
        && previous.as_ref().is_some_and(|snapshot| {
            policy.metadata_ttl > 0
                && crate::cache_ttl::now_unix().saturating_sub(snapshot.fetched_at)
                    < policy.metadata_ttl
        });
    if fresh
        && previous
            .as_ref()
            .is_some_and(|snapshot| snapshot.project.is_none())
    {
        return Err(ProxyError::NotFound);
    }
    let source = repository.breaker_key();
    if let Some(snapshot) = previous
        .as_ref()
        .filter(|_| state.circuit_breaker.source_unavailable(&source))
    {
        let cached = if snapshot.project.is_some() {
            present(snapshot.clone(), true)
        } else {
            Err(unavailable("project source unavailable"))
        };
        progress.send_modify(|progress| progress.result = Some(cached.clone()));
        if state.circuit_breaker.recovery_due(&source) {
            if let Some(_lease) = repository.pypi_refreshes.try_lease(RECOVERY) {
                let snapshot = refresh(state, name, previous, true, progress).await?;
                return present(snapshot, state.circuit_breaker.source_unavailable(&source));
            }
        }
        return cached;
    }
    if let Some(snapshot) = previous.as_ref().filter(|_| fresh) {
        return present(snapshot.clone(), false);
    }
    let snapshot = refresh(state, name, previous, false, progress).await?;
    present(snapshot, state.circuit_breaker.source_unavailable(&source))
}

async fn enqueue(state: &AppState, name: &str) -> Result<RefreshRx, ProxyError> {
    if state.cancel_token.is_cancelled() {
        return Err(unavailable("repository is shutting down"));
    }
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    let crate::config::repositories::RepositoryConfig::Proxy(policy) = &repository.definition
    else {
        return Err(unavailable("missing repository upstream"));
    };
    let timeout = state
        .metadata_deadline
        .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .unwrap_or_else(|| Duration::from_secs(policy.metadata_timeout));
    let flight = repository
        .pypi_refreshes
        .leased(&format!("project:{name}"), "pypi", timeout, || async {
            let (sender, receiver) = tokio::sync::watch::channel(Progress::default());
            Some((receiver, sender))
        })
        .await
        .ok_or_else(|| unavailable("cannot start project refresh"))?;
    let (receiver, sender, lease) = match flight {
        crate::proxy_coalesce::Flight::Follower(receiver) => return Ok(receiver),
        crate::proxy_coalesce::Flight::Leader { value, own, lease } => (value, own, Some(lease)),
        crate::proxy_coalesce::Flight::Alone { value, own } => (value, own, None),
    };
    let mut worker = state.clone();
    worker.metadata_deadline = None;
    worker.repository_mount = None;
    crate::registry::transport::begin_metadata(&mut worker);
    let name = name.to_owned();
    let cancel = state.background_cancel.clone();
    state.background_tasks.spawn(async move {
        let _lease = lease;
        let result = tokio::select! {
            () = cancel.cancelled() => Err(unavailable("project refresh cancelled")),
            result = budget(&worker, resolve(&worker, &name, &sender)) => result,
        };
        sender.send_modify(|progress| {
            if progress.result.is_none()
                || result.is_ok()
                || matches!(result, Err(ProxyError::NotFound))
            {
                progress.result = Some(result);
            }
        });
    });
    Ok(receiver)
}

pub(super) async fn ensure(state: &AppState, name: &str) -> Result<View, ProxyError> {
    let mut operation = state.clone();
    crate::registry::transport::begin_metadata(&mut operation);
    let state = &operation;
    let mut receiver = enqueue(state, name).await?;
    let result = budget(state, async {
        loop {
            let result = receiver.borrow().result.clone();
            if let Some(result) = result {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| unavailable("project refresh cancelled"))?;
        }
    })
    .await;
    let progress = receiver.borrow().clone();
    if let Some(snapshot) = progress.quarantine {
        return present(snapshot, result.as_ref().map_or(true, |view| view.offline));
    }
    match result {
        Ok(view) => Ok(view),
        Err(ProxyError::NotFound) => Err(ProxyError::NotFound),
        Err(error) => match remembered(state, name)
            .or(progress.previous)
            .filter(|snapshot| snapshot.project.is_some())
        {
            Some(snapshot) => present(snapshot, true),
            None => Err(error),
        },
    }
}

pub(super) fn quarantined(snapshot: &Snapshot) -> bool {
    snapshot
        .project
        .as_ref()
        .is_some_and(|project| project.status.status == Status::Quarantined)
}

pub(super) fn failure(error: ProxyError) -> Response {
    match error {
        ProxyError::NotFound => StatusCode::NOT_FOUND.into_response(),
        ProxyError::CircuitOpen(_) => circuit_open_response(RegistryType::PyPI.as_str()),
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn response(
    snapshot: &Snapshot,
    files: &[model::Distribution],
    base: &str,
    headers: &HeaderMap,
    full: bool,
) -> Response {
    let Some(project) = &snapshot.project else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if wants_json(headers) {
        match model::render_json(project, files, base, full, true, None) {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, PEP691_JSON),
                    (header::VARY, "Accept"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    } else {
        (
            [(header::VARY, "Accept")],
            Html(model::render_html(project, files, base, true, None)),
        )
            .into_response()
    }
}

pub(super) async fn known_projects(state: &AppState) -> Result<Vec<String>, ProxyError> {
    let keys = state
        .storage
        .list_internal(SNAPSHOTS)
        .await
        .map_err(|_| unavailable("cannot list project snapshots"))?;
    let mut names = std::collections::BTreeSet::new();
    for key in keys {
        let Some(name) = key
            .strip_prefix(SNAPSHOTS)
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if !model::valid_name(name) {
            continue;
        }
        if stored(state, name)
            .await?
            .is_some_and(|snapshot| snapshot.project.is_some())
        {
            names.insert(name.to_owned());
        }
    }
    if let Some(repository) = &state.repository {
        names.extend(repository.pypi_snapshots.read().keys().cloned());
    }
    names.retain(|name| {
        !crate::curation::is_internal_namespace(
            &state.curation().curation_engine,
            crate::curation::RegistryType::PyPI,
            name,
        )
    });
    Ok(names.into_iter().collect())
}

pub(super) async fn project(
    state: &AppState,
    name: &str,
    base: &str,
    headers: &HeaderMap,
) -> Response {
    if let Some(response) = crate::curation::check_namespace_isolation(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
    ) {
        return response;
    }
    let view = match ensure(state, name).await {
        Ok(view) => view,
        Err(error) => return failure(error),
    };
    project_view(state, name, base, headers, view).await
}

pub(super) async fn project_view(
    state: &AppState,
    name: &str,
    base: &str,
    headers: &HeaderMap,
    view: View,
) -> Response {
    let Some(project) = &view.snapshot.project else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut files = if view.offline {
        let keys = match state.storage.list_with_meta(&format!("pypi/{name}/")).await {
            Ok(keys) => keys.into_iter().collect::<BTreeMap<_, _>>(),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
        let mut files = Vec::new();
        for file in &project.files {
            let key = format!("pypi/{name}/{}", file.filename);
            let Some(meta) = keys.get(&key) else { continue };
            let Ok(Some(ready)) = super::cache::load(state, name, file, Some(meta.size)).await
            else {
                continue;
            };
            let mut file = file.clone();
            file.size = Some(ready.size);
            files.push(file);
        }
        files
    } else {
        project.files.clone()
    };
    for file in &mut files {
        if file
            .core_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.available())
            && !super::transfer::metadata_available(state, name, file, view.offline).await
        {
            file.core_metadata = None;
        }
    }
    let mut result = response(&view.snapshot, &files, base, headers, !view.offline);
    if view.offline {
        result
            .headers_mut()
            .insert("x-nora-stale", axum::http::HeaderValue::from_static("true"));
    }
    result
}

pub(super) async fn prepare_download(
    state: &AppState,
    name: &str,
    prefetch: bool,
    prepared: Option<View>,
) -> Result<Option<View>, Box<Response>> {
    if !is_proxy(state) {
        return Ok(None);
    }
    if let Some(response) = crate::curation::check_namespace_isolation(
        &state.curation().curation_engine,
        crate::curation::RegistryType::PyPI,
        name,
    ) {
        return Err(Box::new(response));
    }
    if let Some(view) = prepared {
        return Ok(Some(view));
    }
    if !prefetch {
        return Ok(None);
    }
    complete_download(state, name, None).await
}

pub(super) async fn complete_download(
    state: &AppState,
    name: &str,
    previous: Option<View>,
) -> Result<Option<View>, Box<Response>> {
    if !is_proxy(state) {
        return Ok(None);
    }
    let view = match previous {
        Some(view) => view,
        None => ensure(state, name)
            .await
            .map_err(|error| Box::new(failure(error)))?,
    };
    if quarantined(&view.snapshot) {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    Ok(Some(view))
}

pub(super) async fn publish_date(state: &AppState, name: &str, filename: &str) -> Option<i64> {
    if state.config.server.trust_upstream_dates {
        let snapshot = stored(state, name).await.ok()??;
        return snapshot
            .project
            .as_ref()?
            .files
            .iter()
            .find(|file| file.filename == filename)
            .and_then(|file| file.upload_time.as_deref())
            .and_then(crate::curation::parse_iso8601_to_unix);
    }
    let bytes = verified(state, &format!("{FIRST_SEEN}{name}.json"))
        .await
        .ok()??;
    let ledger: FirstSeen = serde_json::from_slice(&bytes).ok()?;
    if ledger.source_id != state.repository.as_ref()?.source_id {
        return None;
    }
    ledger.files.get(filename).map(|entry| entry.first_seen)
}
