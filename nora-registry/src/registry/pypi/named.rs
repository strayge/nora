// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! A valid source snapshot carries file constraints and project status together.

use super::model::{self, Project, Status};
use super::*;
use crate::registry::transport::ProxyClient;
use crate::registry::{ProxyError, Revalidation, Validators};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const SNAPSHOTS: &str = ".nora-pypi-index/";
const FIRST_SEEN: &str = ".nora-pypi-first-seen/";

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub schema: u32,
    pub source_id: String,
    pub fetched_at: u64,
    pub validators: Validators,
    pub page_url: String,
    pub content_type: String,
    pub project: Project,
}

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

async fn verified(state: &AppState, key: &str) -> Result<Option<Bytes>, ProxyError> {
    match state.storage.get_verified(key).await {
        Ok(nora_registry::verified::GateOutcome::Verified(blob)) => {
            Ok(Some(nora_registry::verified::verified_body(blob)))
        }
        Ok(_) => Err(unavailable("untrusted project cache record")),
        Err(crate::storage::StorageError::NotFound) => Ok(None),
        Err(_) => Err(unavailable("project cache record unavailable")),
    }
}

pub(super) async fn stored(state: &AppState, name: &str) -> Result<Option<Snapshot>, ProxyError> {
    let Some(repository) = state.repository.as_ref() else {
        return Ok(None);
    };
    let remembered = repository.pypi_snapshots.read().get(name).cloned();
    let bytes = match remembered {
        Some(bytes) => Some(bytes),
        None => verified(state, &format!("{SNAPSHOTS}{name}.json")).await?,
    };
    let Some(bytes) = bytes else { return Ok(None) };
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).map_err(|_| unavailable("invalid project cache record"))?;
    if snapshot.schema != 1
        || snapshot.source_id != repository.source_id
        || snapshot.project.name != name
    {
        return Err(unavailable("project cache source mismatch"));
    }
    Ok(Some(snapshot))
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

async fn request_project(
    state: &AppState,
    name: &str,
    accept: &'static str,
) -> Result<Snapshot, ProxyError> {
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
    let result = crate::registry::proxy_fetch_conditional_extract(
        ProxyClient::for_state(state),
        url.as_str(),
        Duration::from_secs(state.config.pypi.proxy_timeout),
        policy
            .auth
            .as_ref()
            .map(|auth| UpstreamAuth::Basic(auth.expose())),
        &Validators::default(),
        Some(("Accept", accept)),
        |response| async move {
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
                .map_err(|error| ProxyError::Network(error.to_string()))?;
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
                schema: 1,
                source_id: repository.source_id.clone(),
                fetched_at: crate::cache_ttl::now_unix(),
                validators: Validators::default(),
                page_url: page_url.to_string(),
                content_type,
                project,
            })
        },
        &state.circuit_breaker,
        BreakerScope::upstream(RegistryType::PyPI, &policy.upstream),
    )
    .await?;
    match result {
        Revalidation::Modified {
            mut body,
            validators,
        } => {
            body.validators = validators;
            Ok(body)
        }
        Revalidation::NotModified => Err(unavailable("unexpected project NotModified response")),
    }
}

pub(super) async fn fetch(state: &AppState, name: &str) -> Result<Snapshot, ProxyError> {
    let mut operation = state.clone();
    crate::registry::transport::begin_metadata(&mut operation);
    let state = &operation;
    let key = format!("{SNAPSHOTS}{name}.json");
    let lock = state.publish_lock(&key);
    let _guard = match state.metadata_deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, lock.lock())
            .await
            .map_err(|_| unavailable("project operation timed out"))?,
        None => lock.lock().await,
    };
    let snapshot = match request_project(
        state,
        name,
        "application/vnd.pypi.simple.v1+json, text/html;q=0.1",
    )
    .await
    {
        Err(ProxyError::Upstream(406 | 415)) => request_project(state, name, "text/html").await?,
        result => result?,
    };
    let bytes = Bytes::from(
        serde_json::to_vec(&snapshot).map_err(|_| unavailable("cannot encode project snapshot"))?,
    );
    let repository = state
        .repository
        .as_ref()
        .ok_or_else(|| unavailable("missing repository source"))?;
    if snapshot.project.status.status == Status::Quarantined {
        repository
            .pypi_snapshots
            .write()
            .insert(name.into(), bytes.clone());
    }
    record_seen(state, &snapshot.project).await?;
    state
        .storage
        .put(&key, &bytes)
        .await
        .map_err(|_| unavailable("cannot persist project snapshot"))?;
    repository.pypi_snapshots.write().remove(name);
    Ok(snapshot)
}

pub(super) fn quarantined(snapshot: &Snapshot) -> bool {
    snapshot.project.status.status == Status::Quarantined
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
    if wants_json(headers) {
        match model::render_json(&snapshot.project, files, base, full, false, None) {
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
            Html(model::render_html(
                &snapshot.project,
                files,
                base,
                false,
                None,
            )),
        )
            .into_response()
    }
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
    match fetch(state, name).await {
        Ok(snapshot) => response(&snapshot, &snapshot.project.files, base, headers, true),
        Err(ProxyError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            let snapshot = match stored(state, name).await {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => return failure(error),
                Err(error) => return failure(error),
            };
            let keys = match state.storage.list_with_meta(&format!("pypi/{name}/")).await {
                Ok(keys) => keys.into_iter().collect::<BTreeMap<_, _>>(),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            let files = snapshot
                .project
                .files
                .iter()
                .filter_map(|file| {
                    let meta = keys.get(&format!("pypi/{name}/{}", file.filename))?;
                    let mut file = file.clone();
                    file.size = Some(meta.size);
                    Some(file)
                })
                .collect::<Vec<_>>();
            let mut response = response(&snapshot, &files, base, headers, false);
            response
                .headers_mut()
                .insert("x-nora-stale", axum::http::HeaderValue::from_static("true"));
            response
        }
    }
}

pub(super) async fn prepare_download(
    state: &AppState,
    name: &str,
    key: &str,
    prefetch: bool,
) -> Result<Option<Snapshot>, Box<Response>> {
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
    let previous = stored(state, name)
        .await
        .map_err(|error| Box::new(failure(error)))?;
    if previous.as_ref().is_some_and(quarantined) {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    if state.storage.stat(key).await.is_some() {
        return Ok(previous);
    }
    if !prefetch {
        return Ok(None);
    }
    let snapshot = fetch(state, name)
        .await
        .map_err(|error| Box::new(failure(error)))?;
    if quarantined(&snapshot) {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    Ok(Some(snapshot))
}

pub(super) async fn publish_date(state: &AppState, name: &str, filename: &str) -> Option<i64> {
    if state.config.server.trust_upstream_dates {
        let snapshot = stored(state, name).await.ok()??;
        return snapshot
            .project
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
