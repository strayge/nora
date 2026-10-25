// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Serving loaders can rebuild or refresh; browsing reads source-bound records only.

use super::*;
use crate::registry::saved::{Checksum, Distribution, Metadata, Source, SourceState};
use futures::{stream, StreamExt};
use serde_json::Value;

fn coordinate(value: &str) -> bool {
    crate::repository_management::valid_version(value)
}

async fn document(
    state: &AppState,
    name: &str,
    spec: Option<&str>,
    key: &str,
    proxy: bool,
) -> Result<Option<Value>, &'static str> {
    if !state
        .storage
        .exists(key)
        .await
        .map_err(|_| "Saved metadata could not be inspected.")?
    {
        return Ok(None);
    }
    let path = spec
        .map(|spec| format!("{name}/{spec}"))
        .unwrap_or_else(|| name.to_owned());
    if proxy && !cached_copy_trusted(state, &path, key, name, None).await {
        return Err("Saved metadata is unavailable or does not belong to the configured source.");
    }
    let data = state
        .storage
        .get_verified(key)
        .await
        .map_err(|_| "Saved metadata could not be read.")?;
    use nora_registry::verified::{verified_body, GateOutcome};
    let bytes = match data {
        GateOutcome::Verified(blob) => verified_body(blob),
        GateOutcome::Unpinned(blob) => blob.into_inner(),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Saved metadata is malformed.")
}

fn distribution(version: &str, value: &Value, source: &Source) -> Option<Distribution> {
    let dist = value.get("dist")?.as_object()?;
    let mut checksums = Vec::new();
    for (field, algorithm, representation) in
        [("integrity", "SRI", "sri"), ("shasum", "sha1", "hex")]
    {
        if let Some(value) = dist
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            checksums.push(Checksum {
                algorithm: algorithm.into(),
                value: value.into(),
                representation,
                source: source.source.clone(),
                saved_at: source.saved_at,
            });
        }
    }
    if checksums.is_empty() && !dist.get("tarball").is_some_and(Value::is_string) {
        return None;
    }
    Some(Distribution {
        version: version.into(),
        filename: None,
        size: None,
        checksums,
    })
}

fn apply(
    metadata: &mut Metadata,
    value: &Value,
    source: &Source,
    name: &str,
    spec: Option<&str>,
    local: bool,
) -> Result<(), &'static str> {
    if !value.is_object() || (!local && value["name"].as_str() != Some(name)) {
        return Err("Saved metadata has a mismatched package identity.");
    }
    match spec {
        None => {
            let versions = value["versions"]
                .as_object()
                .ok_or("Saved package metadata has no version listing.")?;
            for (version, info) in versions {
                if !coordinate(version) {
                    metadata.warn("Some saved versions have invalid coordinates.");
                    continue;
                }
                metadata.versions.insert(version.clone());
                metadata.files.extend(distribution(version, info, source));
            }
        }
        Some(spec) => {
            let version = if local {
                spec
            } else {
                let version = value["version"]
                    .as_str()
                    .filter(|v| coordinate(v))
                    .ok_or("Saved version metadata has no valid version identity.")?;
                if let Some(requested) = crate::curation::npm_version_key(spec) {
                    if crate::curation::npm_version_key(version).as_ref() != Some(&requested) {
                        return Err("Saved version metadata has a mismatched version identity.");
                    }
                }
                version
            };
            metadata.versions.insert(version.into());
            metadata.files.extend(distribution(version, value, source));
        }
    }
    if metadata.description.is_none() {
        metadata.description = value["description"].as_str().map(str::to_owned);
    }
    if spec.is_none() || local {
        metadata.exhaustive = true;
    }
    Ok(())
}

pub(crate) async fn read(state: &AppState, name: &str) -> Metadata {
    let proxy = state.repository.as_ref().is_some_and(|repository| {
        matches!(
            repository.definition,
            crate::config::repositories::RepositoryConfig::Proxy(_)
        )
    });
    let mut result = Metadata::default();
    let prefix = format!("npm/{name}/");
    let entries = match state.storage.list_with_meta(&prefix).await {
        Ok(entries) => entries,
        Err(_) => {
            result.warn("Saved metadata discovery failed; the version inventory is incomplete.");
            Vec::new()
        }
    };
    let local_packument = !proxy
        && entries
            .iter()
            .any(|(key, _)| *key == published_packument_key(name));
    let mut candidates = Vec::new();
    for (key, meta) in entries {
        let Some(tail) = key.strip_prefix(&prefix) else {
            continue;
        };
        let spec = if proxy {
            tail.strip_prefix("upstream/manifests/")
                .and_then(|v| v.strip_suffix(".json"))
                .or_else(|| tail.strip_suffix("/metadata.json"))
        } else {
            tail.strip_prefix("versions/")
                .and_then(|v| v.strip_suffix(".json"))
        };
        if let Some(spec) = spec {
            if spec.is_empty()
                || spec.contains(['/', '\\'])
                || spec.chars().any(char::is_control)
                || (!proxy && !coordinate(spec))
            {
                result.warn("Some saved metadata coordinates cannot be inspected.");
                continue;
            }
            if !proxy {
                result.versions.insert(spec.into());
            }
            let legacy = proxy && !tail.starts_with("upstream/manifests/");
            candidates.push((key.clone(), spec.to_owned(), Some(meta.modified), legacy));
        }
    }
    if local_packument && candidates.is_empty() {
        result.warn("Saved package metadata exists without readable version records; the inventory is incomplete.");
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    let ttl = state.config.npm.metadata_ttl;
    if proxy {
        for (key, label) in [
            (upstream_packument_key(name), "Saved npm packument"),
            (legacy_cache_key(name), "Saved legacy npm packument"),
        ] {
            let mut source = Source {
                source: label.into(),
                state: SourceState::Missing,
                saved_at: state.storage.stat(&key).await.map(|m| m.modified),
                expired: None,
            };
            match document(state, name, None, &key, true).await {
                Ok(Some(value)) => {
                    source.state = SourceState::Available;
                    source.expired = source
                        .saved_at
                        .map(|at| !crate::cache_ttl::is_within_ttl(at, ttl));
                    match apply(&mut result, &value, &source, name, None, false) {
                        Ok(()) => {
                            result.sources.push(source);
                            break;
                        }
                        Err(error) => {
                            source.state = SourceState::Unavailable;
                            result.warn(error);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    source.state = SourceState::Unavailable;
                    result.warn(error);
                }
            }
            result.sources.push(source);
        }
    }
    let records = stream::iter(candidates)
        .map(|(key, spec, saved_at, legacy)| async move {
            let data = document(state, name, Some(&spec), &key, proxy).await;
            (spec, saved_at, legacy, data)
        })
        .buffered(8)
        .collect::<Vec<_>>()
        .await;
    for (spec, saved_at, legacy, record) in records {
        let mut source = Source {
            source: if legacy {
                format!("Saved legacy npm manifest ({spec})")
            } else if proxy {
                format!("Saved npm manifest ({spec})")
            } else {
                format!("Publisher metadata ({spec})")
            },
            state: SourceState::Available,
            saved_at,
            expired: if proxy {
                saved_at.map(|at| !crate::cache_ttl::is_within_ttl(at, ttl))
            } else {
                None
            },
        };
        match record {
            Ok(Some(value)) => {
                if let Err(error) = apply(&mut result, &value, &source, name, Some(&spec), !proxy) {
                    source.state = SourceState::Unavailable;
                    result.warn(error);
                }
            }
            Ok(None) => {
                source.state = SourceState::Unavailable;
                result.warn("Saved version metadata disappeared during inspection.");
            }
            Err(error) => {
                source.state = SourceState::Unavailable;
                result.warn(error);
            }
        }
        result.sources.push(source);
    }
    if result.sources.is_empty() {
        result.sources.push(Source {
            source: "Publisher metadata".into(),
            state: if result.warnings.is_empty() {
                SourceState::Missing
            } else {
                SourceState::Unavailable
            },
            saved_at: None,
            expired: None,
        });
    }
    result
}
