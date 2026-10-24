// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! A body alone is not an offline candidate: its complete source-bound proof must match.

use super::model::Distribution;
use super::{named, AppState};
use crate::registry::{ProxyError, Validators};
use nora_registry::verified::{verified_body, GateOutcome};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;

pub(super) const READY: &str = ".nora-pypi-ready/";

#[derive(Clone, Serialize)]
pub(super) struct Artifact {
    pub file: Distribution,
    pub parent: Option<Distribution>,
}

impl std::ops::Deref for Artifact {
    type Target = Distribution;
    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl Artifact {
    pub fn distribution(file: Distribution) -> Self {
        Self { file, parent: None }
    }
    pub fn coordinate(&self) -> &Distribution {
        self.parent.as_ref().unwrap_or(&self.file)
    }
    pub fn binding(&self) -> Option<String> {
        let parent = self.parent.as_ref()?;
        let url = (!parent.hashes.keys().any(|algorithm| {
            matches!(
                algorithm.as_str(),
                "sha224" | "sha256" | "sha384" | "sha512"
            )
        }))
        .then_some(&parent.url);
        Some(hex::encode(sha2::Sha256::digest(
            serde_json::to_vec(&(&parent.filename, &parent.hashes, url)).ok()?,
        )))
    }
    pub fn resolve(project: &super::model::Project, filename: &str) -> Option<Self> {
        if let Some(parent) = filename.strip_suffix(".metadata") {
            if !super::is_valid_pypi_filename(parent) {
                return None;
            }
            let parent = project.files.iter().find(|file| file.filename == parent)?;
            Self::sidecar(parent)
        } else {
            project
                .files
                .iter()
                .find(|file| file.filename == filename)
                .cloned()
                .map(Self::distribution)
        }
    }
    pub fn sidecar(parent: &Distribution) -> Option<Self> {
        let metadata = parent
            .core_metadata
            .as_ref()
            .filter(|metadata| metadata.available())?;
        let hashes = match metadata {
            super::model::CoreMetadata::Hashes(hashes) => hashes.clone(),
            _ => BTreeMap::new(),
        };
        let mut url = reqwest::Url::parse(&parent.url).ok()?;
        url.set_path(&format!("{}.metadata", url.path()));
        url.set_fragment(None);
        Some(Self {
            file: Distribution {
                filename: format!("{}.metadata", parent.filename),
                url: url.to_string(),
                hashes,
                requires_python: None,
                yanked: None,
                core_metadata: None,
                size: None,
                upload_time: None,
            },
            parent: Some(parent.clone()),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Ready {
    pub schema: u32,
    pub source_id: String,
    pub name: String,
    pub file: Distribution,
    #[serde(default)]
    pub parent_binding: Option<String>,
    pub size: u64,
    pub hashes: BTreeMap<String, String>,
    pub validators: Validators,
}

fn failure() -> ProxyError {
    ProxyError::Network("invalid or unavailable artifact cache proof".into())
}

pub(super) fn body_key(name: &str, filename: &str) -> String {
    format!("pypi/{name}/{filename}")
}
pub(super) fn ready_key(name: &str, filename: &str) -> String {
    format!("{READY}{name}/{filename}.json")
}

pub(super) fn supported(algorithm: &str) -> bool {
    matches!(
        algorithm,
        "sha224" | "sha256" | "sha384" | "sha512" | "sha1" | "md5"
    )
}

pub(super) fn expected_match(
    file: &Distribution,
    size: u64,
    hashes: &BTreeMap<String, String>,
) -> bool {
    file.size.is_none_or(|expected| expected == size)
        && file.hashes.iter().all(|(algorithm, expected)| {
            !supported(algorithm) || hashes.get(algorithm) == Some(expected)
        })
}

fn same_description(previous: &Distribution, current: &Distribution) -> bool {
    previous.hashes == current.hashes
        && (current.hashes.keys().any(|algorithm| {
            matches!(
                algorithm.as_str(),
                "sha224" | "sha256" | "sha384" | "sha512"
            )
        }) || previous.url == current.url)
}

async fn read_record(
    state: &AppState,
    name: &str,
    filename: &str,
) -> Result<Option<Ready>, ProxyError> {
    match state.storage.get_verified(&ready_key(name, filename)).await {
        Ok(GateOutcome::Verified(blob)) => serde_json::from_slice(&verified_body(blob))
            .map(Some)
            .map_err(|_| failure()),
        Err(crate::storage::StorageError::NotFound) => Ok(None),
        _ => Err(failure()),
    }
}

async fn current(
    state: &AppState,
    name: &str,
    original: &Artifact,
) -> Result<Artifact, ProxyError> {
    let mut metadata_state = state.clone();
    metadata_state.metadata_deadline = None;
    crate::registry::transport::begin_metadata(&mut metadata_state);
    let Some(snapshot) = named::stored(&metadata_state, name).await? else {
        return Err(failure());
    };
    if named::quarantined(&snapshot) {
        return Err(failure());
    }
    let file = snapshot
        .project
        .as_ref()
        .and_then(|project| Artifact::resolve(project, &original.filename))
        .ok_or_else(failure)?;
    if !same_description(original, &file) || original.binding() != file.binding() {
        return Err(failure());
    }
    Ok(file)
}

/// Earlier named caches can acquire proof only after their exact source bytes are reverified.
async fn migrate(
    state: &AppState,
    name: &str,
    file: &Artifact,
) -> Result<Option<Ready>, ProxyError> {
    use tokio::io::AsyncReadExt;
    let Some(expected) = file.hashes.get("sha256") else {
        return Ok(None);
    };
    let key = body_key(name, &file.filename);
    if state.storage.pin(&key).await.as_ref() != Some(expected) {
        return Ok(None);
    }
    let lock = state.publish_lock(&ready_key(name, &file.filename));
    let _guard = lock.lock().await;
    if let Some(ready) = read_record(state, name, &file.filename).await? {
        return Ok(Some(ready));
    }
    let file = current(state, name, file).await?;
    let (size, pin, mut reader) = state
        .storage
        .get_reader(&key)
        .await
        .map_err(|_| failure())?;
    if pin.as_ref() != Some(expected) || file.size.is_some_and(|expected| expected != size) {
        return Ok(None);
    }
    let mut hashers = super::transfer::hashers(&file);
    let mut buffer = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    loop {
        let count = reader.read(&mut buffer).await.map_err(|_| failure())?;
        if count == 0 {
            break;
        }
        read = read.saturating_add(count as u64);
        if read > size {
            return Ok(None);
        }
        for (_, hasher) in &mut hashers {
            hasher.update(&buffer[..count]);
        }
    }
    let hashes: BTreeMap<_, _> = hashers
        .into_iter()
        .map(|(algorithm, hasher)| (algorithm, hex::encode(hasher.finalize())))
        .collect();
    if read != size
        || !expected_match(&file, size, &hashes)
        || hashes.get("sha256") != Some(expected)
    {
        return Ok(None);
    }
    let file = current(state, name, &file).await?;
    if !expected_match(&file, size, &hashes)
        || state.storage.pin(&key).await.as_ref() != Some(expected)
        || state
            .storage
            .stat(&key)
            .await
            .is_none_or(|meta| meta.size != size)
    {
        return Ok(None);
    }
    let ready = Ready {
        schema: 1,
        source_id: state
            .repository
            .as_ref()
            .ok_or_else(failure)?
            .source_id
            .clone(),
        name: name.into(),
        parent_binding: file.binding(),
        file: file.file,
        size,
        validators: Validators {
            etag: Some(format!("\"{expected}\"")),
            last_modified: None,
        },
        hashes,
    };
    if state
        .storage
        .put(
            &ready_key(name, &ready.file.filename),
            &serde_json::to_vec(&ready).map_err(|_| failure())?,
        )
        .await
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(ready))
}

pub(super) async fn load(
    state: &AppState,
    name: &str,
    file: &Distribution,
    known_size: Option<u64>,
) -> Result<Option<Ready>, ProxyError> {
    load_artifact(
        state,
        name,
        &Artifact::distribution(file.clone()),
        known_size,
        true,
    )
    .await
}

pub(super) async fn load_artifact(
    state: &AppState,
    name: &str,
    file: &Artifact,
    known_size: Option<u64>,
    migrate_missing: bool,
) -> Result<Option<Ready>, ProxyError> {
    let repository = state.repository.as_ref().ok_or_else(failure)?;
    let ready = match read_record(state, name, &file.filename).await? {
        Some(ready) => ready,
        None if !migrate_missing => return Ok(None),
        None => match migrate(state, name, file).await? {
            Some(ready) => ready,
            None => return Ok(None),
        },
    };
    if ready.schema != 1
        || ready.source_id != repository.source_id
        || ready.name != name
        || ready.file.filename != file.filename
    {
        return Err(failure());
    }
    if ready.parent_binding != file.binding()
        || !same_description(&ready.file, file)
        || !expected_match(file, ready.size, &ready.hashes)
    {
        return Ok(None);
    }
    let key = body_key(name, &file.filename);
    let size = match known_size {
        Some(size) => size,
        None => match state.storage.stat(&key).await {
            Some(meta) => meta.size,
            None => return Ok(None),
        },
    };
    let Some(sha256) = ready.hashes.get("sha256") else {
        return Err(failure());
    };
    if size != ready.size || state.storage.pin(&key).await.as_ref() != Some(sha256) {
        return Ok(None);
    }
    Ok(Some(ready))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn publish(
    state: &AppState,
    name: &str,
    original: &Artifact,
    size: u64,
    hashes: &BTreeMap<String, String>,
    validators: &Validators,
    staging: &std::path::Path,
) -> Result<(), ProxyError> {
    let key = body_key(name, &original.filename);
    let record = ready_key(name, &original.filename);
    let lock = state.publish_lock(&record);
    let _guard = lock.lock().await;
    let current = current(state, name, original).await?;
    if !expected_match(&current, size, hashes) {
        return Err(failure());
    }
    let repository = state.repository.as_ref().ok_or_else(failure)?;
    let Some(sha256) = hashes.get("sha256") else {
        return Err(failure());
    };
    match state.storage.delete(&record).await {
        Ok(()) | Err(crate::storage::StorageError::NotFound) => {}
        Err(_) => return Err(failure()),
    }
    state
        .storage
        .put_from_path(&key, staging, Some(sha256))
        .await
        .map_err(|_| failure())?;
    if state.storage.pin(&key).await.as_ref() != Some(sha256)
        || state
            .storage
            .stat(&key)
            .await
            .is_none_or(|meta| meta.size != size)
    {
        return Err(failure());
    }
    let ready = Ready {
        schema: 1,
        source_id: repository.source_id.clone(),
        name: name.into(),
        parent_binding: current.binding(),
        file: current.file,
        size,
        hashes: hashes.clone(),
        validators: validators.clone(),
    };
    state
        .storage
        .put(&record, &serde_json::to_vec(&ready).map_err(|_| failure())?)
        .await
        .map_err(|_| failure())?;
    state.repo_index.invalidate("pypi");
    Ok(())
}
