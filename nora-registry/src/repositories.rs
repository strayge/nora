// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Named runtime state is shared between aliases, never between repository identities.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::Instrument;

use crate::config::repositories::{RepositoryConfig, RepositoryFormat};
use crate::config::{Config, NpmConfig, NpmProxy, NpmProxyEntry, PypiConfig};
use crate::proxy_coalesce::InflightMap;
use crate::repo_index::RepoIndex;
use crate::storage::{Storage, StorageError};
use crate::{AppState, PublishLocks};

#[derive(Default)]
pub(crate) struct RepositoryCatalogue {
    members: BTreeMap<String, Arc<Repository>>,
}

pub(crate) struct Repository {
    pub name: String,
    pub definition: RepositoryConfig,
    pub source_id: String,
    pub storage: Storage,
    pub transport: Option<crate::registry::transport::RepositoryTransport>,
    config: Arc<Config>,
    index: Arc<RepoIndex>,
    locks: PublishLocks,
    coalesce: InflightMap<Bytes>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    format: RepositoryFormat,
    kind: String,
}

impl RepositoryCatalogue {
    /// A missing identity record may describe a new namespace, never an implicit import.
    pub(crate) async fn open(config: &Config, root: &Storage) -> Result<Self, String> {
        Self::load(config, root, true).await
    }

    /// An advisory inventory scan must neither delay startup nor invalidate good identities.
    pub(crate) async fn warn_retained(&self, root: &Storage) {
        match self.retained_namespaces(root).await {
            Ok(namespaces) => {
                for namespace in namespaces {
                    tracing::warn!(
                        namespace,
                        "Inactive repository namespace retained; operator cleanup required"
                    );
                }
            }
            Err(message) => {
                tracing::warn!(error = %message, "Retained repository inventory unavailable")
            }
        }
    }

    /// Dry-run maintenance must not create identity files for empty repositories.
    pub(crate) async fn inspect(config: &Config, root: &Storage) -> Result<Self, String> {
        Self::load(config, root, false).await
    }

    async fn load(config: &Config, root: &Storage, create: bool) -> Result<Self, String> {
        let mut members = BTreeMap::new();
        for (name, definition) in &config.repositories {
            let kind = match definition {
                RepositoryConfig::Local(_) => "local",
                RepositoryConfig::Proxy(_) => "proxy",
                RepositoryConfig::Group(_) => continue,
            };
            let prefix = format!("repositories/{name}");
            let key = format!("{prefix}/.nora-repository.json");
            let descriptor = Descriptor {
                format: definition.format(),
                kind: kind.into(),
            };
            match root.get(&key).await {
                Ok(bytes) => {
                    let previous: Descriptor = serde_json::from_slice(&bytes)
                        .map_err(|_| format!("repository {name}: invalid stored identity"))?;
                    if descriptor != previous {
                        return Err(format!("repository {name}: stored format/kind differs; use a new repository name"));
                    }
                }
                Err(StorageError::NotFound) => {
                    if !root
                        .list(&format!("{prefix}/"))
                        .await
                        .map_err(|_| format!("repository {name}: cannot inspect namespace"))?
                        .is_empty()
                        || !root
                            .list_internal(&format!("{prefix}/"))
                            .await
                            .map_err(|_| {
                                format!("repository {name}: cannot inspect internal namespace")
                            })?
                            .is_empty()
                    {
                        return Err(format!(
                            "repository {name}: namespace has data but no trusted identity"
                        ));
                    }
                    let bytes = serde_json::to_vec(&descriptor)
                        .map_err(|_| format!("repository {name}: cannot encode identity"))?;
                    if create {
                        root.put(&key, &bytes)
                            .await
                            .map_err(|_| format!("repository {name}: cannot persist identity"))?;
                    }
                }
                Err(_) => return Err(format!("repository {name}: cannot verify stored identity")),
            }
            let source_id = match definition {
                RepositoryConfig::Proxy(proxy) => {
                    let mut url = reqwest::Url::parse(&proxy.upstream)
                        .map_err(|_| format!("repository {name}: invalid upstream"))?;
                    let _ = url.set_username("");
                    let _ = url.set_password(None);
                    hex::encode(Sha256::digest(
                        url.as_str().trim_end_matches('/').as_bytes(),
                    ))
                }
                _ => "local".into(),
            };
            let storage = root
                .scoped(&format!("{prefix}/sources/{source_id}"))
                .map_err(|_| format!("repository {name}: invalid namespace"))?;
            let transport = match definition {
                RepositoryConfig::Proxy(policy) => {
                    if !policy.tls_verify {
                        tracing::warn!(repository = %name, "TLS certificate verification disabled for configured upstream origin");
                    }
                    Some(
                        crate::registry::transport::RepositoryTransport::new(policy, &config.tls)
                            .map_err(|error| format!("repository {name}: {error}"))?,
                    )
                }
                _ => None,
            };
            members.insert(
                name.clone(),
                Arc::new(Repository {
                    name: name.clone(),
                    definition: definition.clone(),
                    source_id,
                    storage,
                    transport,
                    config: Arc::new(protocol_config(config, definition)),
                    index: Arc::new(RepoIndex::new()),
                    locks: Arc::default(),
                    coalesce: InflightMap::new(),
                }),
            );
        }
        Ok(Self { members })
    }

    #[allow(dead_code)] // Route selection is wired in the next slice.
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<Repository>> {
        self.members.get(name)
    }

    pub(crate) fn members(&self) -> impl Iterator<Item = &Arc<Repository>> {
        self.members.values()
    }

    pub(crate) fn registry_for_path(
        &self,
        path: &str,
        enabled: &std::collections::HashSet<crate::registry_type::RegistryType>,
    ) -> Option<crate::registry_type::RegistryType> {
        self.mounted_members(enabled)
            .find(|member| {
                member
                    .definition
                    .paths()
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
            })
            .map(|member| member.registry_type())
    }

    fn mounted_members<'a>(
        &'a self,
        enabled: &'a std::collections::HashSet<crate::registry_type::RegistryType>,
    ) -> impl Iterator<Item = &'a Arc<Repository>> + 'a {
        self.members()
            .filter(move |member| enabled.contains(&member.registry_type()))
    }

    pub(crate) fn is_npm_audit(
        &self,
        path: &str,
        enabled: &std::collections::HashSet<crate::registry_type::RegistryType>,
    ) -> bool {
        self.mounted_members(enabled)
            .filter(|member| member.definition.format() == RepositoryFormat::Npm)
            .any(|member| {
                member.definition.paths().iter().any(|prefix| {
                    path.strip_prefix(prefix).is_some_and(|path| {
                        matches!(
                            path,
                            "-/npm/v1/security/advisories/bulk"
                                | "-/npm/v1/security/audits/quick"
                                | "-/npm/v1/security/audits"
                        )
                    })
                })
            })
    }

    pub(crate) fn routes(&self, root: &AppState) -> axum::Router<AppState> {
        let mut routes = axum::Router::new();
        for member in self.mounted_members(&root.enabled_registries) {
            let mut aliases = axum::Router::new();
            for prefix in member.definition.paths() {
                let publish = matches!(member.definition, RepositoryConfig::Local(_));
                aliases = aliases.merge(match member.definition.format() {
                    RepositoryFormat::Pypi => crate::registry::pypi_routes_at(prefix, publish),
                    RepositoryFormat::Npm => crate::registry::npm::routes_at(prefix, publish),
                });
            }
            routes = routes.merge(aliases.with_state(member.state(root)));
        }
        routes.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static("sandbox"),
        ))
    }

    async fn retained_namespaces(&self, root: &Storage) -> Result<Vec<String>, String> {
        let mut keys = root
            .list("repositories/")
            .await
            .map_err(|_| "cannot enumerate retained repository namespaces")?;
        keys.extend(
            root.list_internal("repositories/")
                .await
                .map_err(|_| "cannot enumerate repository identity records")?,
        );
        let mut retained = std::collections::BTreeSet::new();
        for key in keys {
            let parts: Vec<_> = key.split('/').collect();
            let Some(name) = parts.get(1) else { continue };
            match self.members.get(*name) {
                None => {
                    retained.insert(format!("repositories/{name}/"));
                }
                Some(member)
                    if parts.get(2) == Some(&"sources")
                        && parts
                            .get(3)
                            .is_some_and(|source| *source != member.source_id) =>
                {
                    retained.insert(format!("repositories/{name}/sources/{}/", parts[3]));
                }
                Some(_) => {}
            }
        }
        Ok(retained.into_iter().collect())
    }

    pub(crate) async fn extend_gc(
        &self,
        config: &Config,
        dry_run: bool,
        total: &mut crate::gc::GcResult,
    ) {
        for member in self.members() {
            let result = crate::gc::run_gc(
                &member.storage,
                &member.locks,
                dry_run,
                config.gc.grace_secs,
                matches!(member.definition, RepositoryConfig::Proxy(_)),
                0,
            )
            .instrument(tracing::info_span!("repository_gc", repository = %member.name))
            .await;
            let changed = result.removed_anything();
            total.total_candidates += result.total_candidates;
            total.orphaned += result.orphaned;
            total.deleted += result.deleted;
            total.bytes_freed += result.bytes_freed;
            total.metadata_phantoms_removed += result.metadata_phantoms_removed;
            total.skipped_recent += result.skipped_recent;
            total.stat_failures += result.stat_failures;
            total.duration_secs += result.duration_secs;
            total.orphan_keys.extend(
                result
                    .orphan_keys
                    .into_iter()
                    .map(|key| format!("{}:{key}", member.name)),
            );
            if !dry_run && changed {
                member.index.invalidate_all();
            }
        }
    }

    pub(crate) async fn extend_retention(
        &self,
        config: &Config,
        dry_run: bool,
        total: &mut crate::retention::RetentionResult,
    ) {
        for member in self.members() {
            let result = crate::retention::run_retention(
                &member.storage,
                &member.locks,
                None,
                &config.retention.rules,
                dry_run,
            )
            .instrument(tracing::info_span!("repository_retention", repository = %member.name))
            .await;
            total.planned += result.planned;
            total.deleted_keys += result.deleted_keys;
            total.bytes_freed += result.bytes_freed;
            total.duration_secs += result.duration_secs;
            total.plans.extend(
                result
                    .plans
                    .into_iter()
                    .map(|(name, plans)| (format!("{}:{name}", member.name), plans)),
            );
            if !dry_run && result.deleted_keys > 0 {
                member.index.invalidate_all();
            }
        }
    }

    pub(crate) async fn gc(&self, root: &AppState) {
        for member in self.members() {
            let state = member.state(root);
            let result = crate::gc_pass(
                &state.storage,
                &state.publish_locks,
                &state.repo_index,
                root.config.gc.dry_run,
                root.config.gc.grace_secs,
                matches!(member.definition, RepositoryConfig::Proxy(_)),
                0,
            )
            .instrument(tracing::info_span!("repository_gc", repository = %member.name))
            .await;
            tracing::info!(repository = %member.name, source = %member.source_id,
                deleted = result.deleted, "Named repository GC finished");
        }
    }

    pub(crate) async fn retain(&self, root: &AppState) {
        for member in self.members() {
            let state = member.state(root);
            let result = crate::retention_pass(
                &state.storage,
                &state.publish_locks,
                &state.repo_index,
                state.signer.as_deref(),
                &root.config.retention.rules,
                root.config.retention.dry_run,
                &root.audit,
            )
            .instrument(tracing::info_span!("repository_retention", repository = %member.name))
            .await;
            tracing::info!(repository = %member.name, deleted = result.deleted_keys,
                "Named repository retention finished");
        }
    }

    pub(crate) fn invalidate(&self, target: Option<crate::registry_type::RegistryType>) {
        for member in self
            .members()
            .filter(|member| target.is_none_or(|rt| rt == member.registry_type()))
        {
            member.index.invalidate_all();
        }
    }

    pub(crate) async fn warm(&self, target: Option<crate::registry_type::RegistryType>) {
        for member in self
            .members()
            .filter(|member| target.is_none_or(|rt| rt == member.registry_type()))
        {
            member
                .index
                .get(member.registry_type().as_str(), &member.storage)
                .await;
        }
    }
}

impl Repository {
    pub(crate) fn registry_type(&self) -> crate::registry_type::RegistryType {
        match self.definition.format() {
            RepositoryFormat::Npm => crate::registry_type::RegistryType::Npm,
            RepositoryFormat::Pypi => crate::registry_type::RegistryType::PyPI,
        }
    }

    /// Shared security and shutdown services remain attached to the running server.
    pub(crate) fn state(self: &Arc<Self>, root: &AppState) -> AppState {
        debug_assert!(
            root.repository.is_none(),
            "repository views require root state"
        );
        let mut view = root.clone();
        view.repository = Some(Arc::clone(self));
        view.storage = self.storage.clone();
        view.config = Arc::clone(&self.config);
        view.repo_index = Arc::clone(&self.index);
        view.publish_locks = Arc::clone(&self.locks);
        view.proxy_coalesce = self.coalesce.clone();
        view
    }
}

fn protocol_config(root: &Config, definition: &RepositoryConfig) -> Config {
    let mut config = root.clone();
    match definition.format() {
        RepositoryFormat::Npm => {
            let mut npm = NpmConfig {
                proxy: None,
                ..NpmConfig::default()
            };
            if let RepositoryConfig::Proxy(proxy) = definition {
                npm.metadata_ttl = i64::try_from(proxy.metadata_ttl).unwrap_or(i64::MAX);
                npm.proxies = vec![NpmProxyEntry::Full(NpmProxy {
                    url: proxy.upstream.clone(),
                    scopes: Vec::new(),
                    auth: proxy.auth.clone(),
                    auth_bearer_env: proxy.auth_bearer_env.clone(),
                    auth_bearer: proxy.auth_bearer.clone(),
                })];
            }
            config.npm = npm;
        }
        RepositoryFormat::Pypi => {
            let mut pypi = PypiConfig {
                proxy: None,
                ..PypiConfig::default()
            };
            if let RepositoryConfig::Proxy(proxy) = definition {
                pypi.proxy = Some(proxy.upstream.clone());
                pypi.proxy_auth.clone_from(&proxy.auth);
            }
            config.pypi = pypi;
        }
    }
    config
}

#[cfg(test)]
mod tests;
