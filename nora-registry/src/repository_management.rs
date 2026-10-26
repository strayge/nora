// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Explicit local cleanup, separate from the public browse surface and protocol publication.

use crate::{
    audit::AuditEntry,
    auth::{enforce_namespace_scope, AuthenticatedRole, AuthenticatedUser, NamespaceAuthority},
    config::repositories::{RepositoryConfig, RepositoryFormat},
    registry::{npm, pypi},
    registry_type::RegistryType,
    storage::{FileMeta, StorageError},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::delete,
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/repositories/{repository}/packages/{*name}",
        delete(remove),
    )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    version: Option<String>,
}

pub(crate) fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && !version.contains(['/', '\\', '\0'])
        && !version.contains("..")
        && !version.chars().any(char::is_control)
}

pub(crate) fn canonical_name(format: RepositoryFormat, name: &str) -> Option<String> {
    match format {
        RepositoryFormat::Docker => None,
        RepositoryFormat::Npm => npm::valid_package_name(name).then(|| name.to_owned()),
        RepositoryFormat::Pypi => crate::registry::canonical_pypi_project(name),
    }
}

pub(crate) fn registry(format: RepositoryFormat) -> RegistryType {
    format.registry_type()
}

#[derive(Clone)]
pub(crate) struct Distribution {
    pub key: String,
    pub version: Option<String>,
    pub meta: FileMeta,
}

pub(crate) struct Inventory {
    pub keys: BTreeSet<String>,
    pub files: Vec<Distribution>,
    pub releases: BTreeSet<String>,
    pub unassigned: BTreeSet<String>,
}

impl Inventory {
    /// Body and orphan companion discovery share the same exact release matcher.
    pub(crate) async fn read(
        state: &AppState,
        format: RepositoryFormat,
        name: &str,
    ) -> Result<Self, StorageError> {
        let prefix = format!("{}/{name}/", registry(format).as_str());
        let entries = state.storage.list_with_meta(&prefix).await?;
        let mut keys: BTreeSet<_> = entries.iter().map(|(key, _)| key.clone()).collect();
        match format {
            RepositoryFormat::Docker => unreachable!("Docker has no package-file inventory"),
            RepositoryFormat::Npm => {
                keys.extend(
                    state
                        .storage
                        .list_internal(&npm::origin_key(&prefix))
                        .await?,
                );
            }
            RepositoryFormat::Pypi => {
                keys.extend(
                    state
                        .storage
                        .list(&format!("{}{name}/", pypi::HASH_MARKER_PREFIX))
                        .await?,
                );
                keys.extend(
                    state
                        .storage
                        .list_internal(&format!("{}{name}/", pypi::READY_PREFIX))
                        .await?,
                );
            }
        }
        let mut files = Vec::new();
        let mut releases = BTreeSet::new();
        let mut unassigned = BTreeSet::new();
        for (key, meta) in entries {
            if let Some(version) = key
                .strip_prefix(&format!("npm/{name}/versions/"))
                .and_then(|tail| tail.strip_suffix(".json"))
            {
                releases.insert(version.to_owned());
            }
            let is_body = match format {
                RepositoryFormat::Docker => false,
                RepositoryFormat::Npm => key
                    .strip_prefix(&format!("npm/{name}/tarballs/"))
                    .is_some_and(|file| file.ends_with(".tgz")),
                RepositoryFormat::Pypi => key.strip_prefix(&prefix).is_some_and(|file| {
                    !file.contains('/')
                        && nora_registry::pypi_files::served_extension(file).is_some()
                }),
            };
            let version = version_for_key(format, name, &key);
            let is_attachment = format == RepositoryFormat::Npm
                && key
                    .strip_prefix(&format!("npm/{name}/tarballs/"))
                    .is_some_and(|file| !file.ends_with(".sha256"));
            if version.is_none() && (is_body || is_attachment) {
                unassigned.insert(key.clone());
            }
            if is_body {
                releases.extend(version.iter().cloned());
                files.push(Distribution { key, version, meta });
            }
        }
        Ok(Self {
            keys,
            files,
            releases,
            unassigned,
        })
    }

    pub(crate) fn body_size(&self, key: &str) -> Option<u64> {
        self.files
            .iter()
            .find(|file| file.key == key)
            .map(|file| file.meta.size)
    }
}

pub(crate) fn version_for_key(format: RepositoryFormat, name: &str, key: &str) -> Option<String> {
    match format {
        RepositoryFormat::Docker => None,
        RepositoryFormat::Npm => {
            let key = key.strip_prefix(".nora-origins/").unwrap_or(key);
            let file = key.strip_prefix(&format!("npm/{name}/tarballs/"))?;
            let file = file.strip_suffix(".sha256").unwrap_or(file);
            crate::curation::parse_npm_tarball_version(name, file)
        }
        RepositoryFormat::Pypi => {
            let file =
                if let Some(rest) = key.strip_prefix(&format!("{}{name}/", pypi::READY_PREFIX)) {
                    rest.strip_suffix(".json")?
                } else if let Some(rest) =
                    key.strip_prefix(&format!("{}{name}/", pypi::HASH_MARKER_PREFIX))
                {
                    rest.split('/').next()?
                } else {
                    key.strip_prefix(&format!("pypi/{name}/"))?
                };
            let file = file.strip_suffix(".sha256").unwrap_or(file);
            let file = file.strip_suffix(".metadata").unwrap_or(file);
            nora_registry::pypi_files::parse_pypi_version(name, file)
        }
    }
}

#[derive(Default, Serialize, utoipa::ToSchema)]
pub(crate) struct DeleteResult {
    pub repository: String,
    pub package: String,
    pub version: Option<String>,
    pub status: String,
    pub package_removed: bool,
    pub deleted_files: usize,
    pub deleted_bytes: u64,
    pub residual_files: usize,
    #[serde(skip)]
    deleted_objects: usize,
}

pub(crate) struct Operation {
    pub state: AppState,
    pub name: String,
    pub version: Option<String>,
    pub result: DeleteResult,
    pub started: bool,
}

impl Operation {
    pub(crate) async fn delete(
        &mut self,
        key: &str,
        size: Option<u64>,
    ) -> Result<(), StorageError> {
        if self.state.background_cancel.is_cancelled() {
            return Err(StorageError::Network("shutdown interrupted cleanup".into()));
        }
        self.started = true;
        // Never cancel an issued call: its storage work may outlive its future.
        match self.state.storage.delete(key).await {
            Ok(()) => {
                self.result.deleted_objects += 1;
                if let Some(size) = size {
                    self.result.deleted_files += 1;
                    self.result.deleted_bytes += size;
                }
                Ok(())
            }
            Err(StorageError::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if self.started {
            self.state.repo_index.invalidate(
                registry(
                    self.state
                        .repository
                        .as_ref()
                        .expect("member state")
                        .definition
                        .format(),
                )
                .as_str(),
            );
        }
    }
}

async fn remove(
    State(root): State<AppState>,
    Path((repository, name)): Path<(String, String)>,
    Query(query): Query<DeleteQuery>,
    role: Option<Extension<AuthenticatedRole>>,
    Extension(user): Extension<AuthenticatedUser>,
    Extension(authority): Extension<NamespaceAuthority>,
    headers: HeaderMap,
) -> Response {
    if root.config.auth.enabled && !role.is_some_and(|Extension(role)| role.0.can_write()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if headers.get("HX-Request").and_then(|v| v.to_str().ok()) != Some("true") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(definition) = root.config.repositories.get(&repository) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let format = definition.format();
    if !root.enabled_registries.contains(&registry(format)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if format == RepositoryFormat::Docker || !matches!(definition, RepositoryConfig::Local(_)) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(name) = canonical_name(format, &name) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if query
        .version
        .as_deref()
        .is_some_and(|version| !valid_version(version))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if enforce_namespace_scope(&authority, &name).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(gate) = root.cleanup_lock.clone().try_lock_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [("Retry-After", "2")], Json(serde_json::json!({"error":"maintenance_busy","message":"Maintenance is running; retry shortly."}))).into_response();
    };
    let Some(member) = root.repositories.get(&repository) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let state = member.state(&root);
    let task = root.background_tasks.spawn(async move {
        let _gate = gate;
        let mut operation = Operation {
            state,
            name: name.clone(),
            version: query.version.clone(),
            result: DeleteResult { repository, package: name, version: query.version, ..Default::default() },
            started: false,
        };
        let outcome = match format {
            RepositoryFormat::Docker => unreachable!("Docker is excluded from package deletion"),
            RepositoryFormat::Npm => npm::deletion::delete(&mut operation).await,
            RepositoryFormat::Pypi => pypi::deletion::delete(&mut operation).await,
        };
        let mut status = StatusCode::OK;
        let body = match outcome {
            Ok(()) => {
                operation.result.status = if operation.result.deleted_objects == 0 { "already_absent" } else { "deleted" }.into();
                serde_json::to_value(&operation.result).expect("serializable delete result")
            }
            Err(error) => {
                tracing::error!(%error, package=%operation.name, "Local package deletion incomplete");
                status = if operation.started { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::SERVICE_UNAVAILABLE };
                serde_json::json!({"error":"delete_incomplete","message":"Deletion could not complete. Some files may already be gone; retry the same target.","progress":operation.result})
            }
        };
        operation.state.log_audit(AuditEntry::new("delete", &user.0, &operation.name, registry(format).as_str(), &format!("version={:?} status={} files={} bytes={}", operation.version, status.as_u16(), operation.result.deleted_files, operation.result.deleted_bytes)));
        (status, Json(body)).into_response()
    });
    match task.await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Deletion interrupted; refresh and retry.",
        )
            .into_response(),
    }
}

/// Grouping is read-only so browser and mutation use the same release identity.
pub(crate) fn release_files(inventory: &Inventory) -> BTreeMap<String, (usize, u64)> {
    let mut releases: BTreeMap<_, _> = inventory
        .releases
        .iter()
        .map(|version| (version.clone(), (0, 0)))
        .collect();
    for file in &inventory.files {
        if let Some(version) = &file.version {
            let entry = releases.entry(version.clone()).or_default();
            entry.0 += 1;
            entry.1 += file.meta.size;
        }
    }
    releases
}
