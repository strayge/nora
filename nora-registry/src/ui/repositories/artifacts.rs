// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! File observations and saved advertisements are associated, never verified here.

use super::{components, PackageQuery, StoredVersion};
use crate::{
    config::repositories::RepositoryFormat,
    registry::saved::{Checksum, Source, SourceState},
    AppState,
};
use axum::http::StatusCode;
use base64::Engine as _;
use futures::{stream, StreamExt};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(super) struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub page: usize,
    pub limit: usize,
}

fn paginate<T>(items: Vec<T>, page: usize, limit: usize) -> Page<T> {
    let total = items.len();
    Page {
        items: items
            .into_iter()
            .skip(page.saturating_sub(1).saturating_mul(limit))
            .take(limit)
            .collect(),
        total,
        page,
        limit,
    }
}

#[derive(Serialize)]
pub(super) struct File {
    pub filename: Option<String>,
    pub version: Option<String>,
    pub retained: &'static str,
    pub stored_size: Option<u64>,
    pub updated: Option<String>,
    pub advertised: Option<bool>,
    pub advertised_size: Option<u64>,
    pub checksums: Vec<Checksum>,
    pub checksum_conflict: bool,
    pub warnings: Vec<String>,
    #[serde(skip)]
    key: Option<String>,
    #[serde(skip)]
    sidecar: bool,
}

#[derive(Serialize)]
pub(super) struct Version {
    pub version: String,
    pub stored_files: Option<usize>,
    pub advertised_only_files: Option<usize>,
    pub stored_size: Option<u64>,
    pub filename_only: bool,
}

#[derive(Serialize)]
pub(super) struct Inventory {
    pub complete: bool,
    pub stored_available: bool,
    pub metadata_complete: bool,
    pub advertisements_enumerated: bool,
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
    pub versions: Page<Version>,
    pub other_files: Page<File>,
    pub selected_version: Option<String>,
    pub files: Option<Page<File>>,
    #[serde(skip)]
    pub legacy: Vec<StoredVersion>,
    #[serde(skip)]
    pub description: Option<String>,
}

fn version_spelling(version: &str, authoritative: Option<&Vec<String>>, local: bool) -> String {
    if local {
        return version.into();
    }
    let Some(versions) = authoritative else {
        return version.into();
    };
    if versions.iter().any(|v| v == version) {
        return version.into();
    }
    let key = crate::curation::pep440_key(version);
    let candidates: Vec<_> = versions
        .iter()
        .filter(|v| key.is_some() && crate::curation::pep440_key(v) == key)
        .collect();
    if let [only] = candidates.as_slice() {
        (*only).clone()
    } else {
        version.into()
    }
}

fn version_order(format: RepositoryFormat, a: &str, b: &str) -> std::cmp::Ordering {
    let numeric = |version: &str| {
        version
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|part| part.parse::<u64>().ok())
            .collect::<Vec<_>>()
    };
    if format == RepositoryFormat::Npm {
        numeric(b).cmp(&numeric(a)).then_with(|| b.cmp(a))
    } else {
        b.cmp(a)
    }
}

fn empty_file(version: Option<String>, advertised: Option<bool>) -> File {
    File {
        filename: None,
        version,
        retained: "unknown",
        stored_size: None,
        updated: None,
        advertised,
        advertised_size: None,
        checksums: Vec::new(),
        checksum_conflict: false,
        warnings: Vec::new(),
        key: None,
        sidecar: false,
    }
}

fn digest_values(checksum: &Checksum) -> BTreeMap<String, BTreeSet<Vec<u8>>> {
    let mut values: BTreeMap<String, BTreeSet<Vec<u8>>> = BTreeMap::new();
    if checksum.representation == "hex" {
        if let Ok(bytes) = hex::decode(&checksum.value) {
            let expected = match checksum.algorithm.to_ascii_lowercase().as_str() {
                "sha1" => 20,
                "sha256" => 32,
                "sha384" => 48,
                "sha512" => 64,
                "md5" => 16,
                _ => bytes.len(),
            };
            if !bytes.is_empty() && bytes.len() == expected {
                values
                    .entry(checksum.algorithm.to_ascii_lowercase())
                    .or_default()
                    .insert(bytes);
            }
        }
    } else if checksum.representation == "sri" {
        for token in checksum.value.split_whitespace() {
            let Some((algorithm, value)) = token.split_once('-') else {
                continue;
            };
            let value = value.split('?').next().unwrap_or(value);
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(value) {
                let size = match algorithm {
                    "md5" => 16,
                    "sha1" => 20,
                    "sha256" => 32,
                    "sha384" => 48,
                    "sha512" => 64,
                    _ => bytes.len(),
                };
                if !bytes.is_empty()
                    && bytes.len() == size
                    && algorithm.bytes().all(|b| b.is_ascii_alphanumeric())
                {
                    values.entry(algorithm.into()).or_default().insert(bytes);
                }
            }
        }
    }
    values
}

fn checksum_order(value: &Checksum) -> (u8, &str, &str, &str) {
    let rank = if value.source == "Storage pin" {
        0
    } else if value.source == "Stored SHA-256 sidecar" {
        1
    } else if digest_values(value).contains_key("sha256") {
        2
    } else if digest_values(value).contains_key("sha512") {
        3
    } else {
        4
    };
    (rank, &value.algorithm, &value.source, &value.value)
}

async fn checksums(state: &AppState, mut file: File) -> File {
    if let Some(key) = &file.key {
        if let Some(value) = state.storage.pin(key).await {
            file.checksums.push(Checksum {
                algorithm: "sha256".into(),
                value,
                representation: "hex",
                source: "Storage pin".into(),
                saved_at: None,
            });
        }
        if file.sidecar {
            match state.storage.get(&format!("{key}.sha256")).await {
                Ok(bytes) => match std::str::from_utf8(&bytes).ok().map(str::trim) {
                    Some(value)
                        if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    {
                        file.checksums.push(Checksum {
                            algorithm: "sha256".into(),
                            value: value.to_ascii_lowercase(),
                            representation: "hex",
                            source: "Stored SHA-256 sidecar".into(),
                            saved_at: None,
                        })
                    }
                    _ => file
                        .warnings
                        .push("Stored checksum sidecar is malformed.".into()),
                },
                Err(_) => file
                    .warnings
                    .push("Stored checksum sidecar could not be read.".into()),
            }
        }
    }
    file.checksums.retain_mut(|checksum| {
        let values = digest_values(checksum);
        let valid = !values.is_empty();
        if valid && checksum.representation == "sri" {
            checksum.algorithm = values.into_keys().collect::<Vec<_>>().join(", ");
        }
        if !valid {
            file.warnings.push(
                "Some recorded checksums are malformed or use an unsupported representation."
                    .into(),
            );
        }
        valid
    });
    file.checksums.sort_by(|a, b| {
        checksum_order(a)
            .cmp(&checksum_order(b))
            .then(a.saved_at.cmp(&b.saved_at))
    });
    file.checksums.dedup_by(|a, b| {
        a.algorithm == b.algorithm
            && a.value == b.value
            && a.source == b.source
            && a.saved_at == b.saved_at
    });
    let values: Vec<_> = file.checksums.iter().map(digest_values).collect();
    file.checksum_conflict = values.iter().enumerate().any(|(i, left)| {
        values[i + 1..].iter().any(|right| {
            left.iter()
                .any(|(algorithm, a)| right.get(algorithm).is_some_and(|b| a.is_disjoint(b)))
        })
    });
    file.warnings.sort();
    file.warnings.dedup();
    file
}

async fn hydrate(state: &AppState, page: Page<File>) -> Page<File> {
    let items = stream::iter(page.items)
        .map(|file| checksums(state, file))
        .buffered(8)
        .collect()
        .await;
    Page {
        items,
        total: page.total,
        page: page.page,
        limit: page.limit,
    }
}

pub(super) async fn read(
    state: &AppState,
    format: RepositoryFormat,
    name: &str,
    query: &PackageQuery,
) -> Result<Inventory, StatusCode> {
    if query
        .version
        .as_deref()
        .is_some_and(|v| !crate::repository_management::valid_version(v))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let local = state.repository.as_ref().is_some_and(|r| {
        matches!(
            r.definition,
            crate::config::repositories::RepositoryConfig::Local(_)
        )
    });
    let prefix = match format {
        RepositoryFormat::Docker => return Err(StatusCode::BAD_REQUEST),
        RepositoryFormat::Npm => format!("npm/{name}/tarballs/"),
        RepositoryFormat::Pypi => format!("pypi/{name}/"),
    };
    let metadata = async {
        match format {
            RepositoryFormat::Docker => unreachable!("Docker has no distribution inventory"),
            RepositoryFormat::Npm => crate::registry::npm::saved::read(state, name).await,
            RepositoryFormat::Pypi => crate::registry::pypi::saved::read(state, name).await,
        }
    };
    let (listing, metadata) = tokio::join!(state.storage.list_with_meta(&prefix), metadata);
    let stored_available = listing.is_ok();
    let metadata_complete = metadata.complete();
    let mut warnings: Vec<String> = metadata.warnings.iter().map(|v| (*v).into()).collect();
    if !stored_available {
        warnings.push(
            "Stored files could not be listed; retained status and counts are unknown.".into(),
        );
    }
    let metadata_available = metadata
        .sources
        .iter()
        .any(|source| matches!(source.state, SourceState::Available | SourceState::Absent));
    if !stored_available
        && !metadata_available
        && metadata.versions.is_empty()
        && !(local && query.version.is_none())
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let advertisement_known = metadata_complete && metadata.exhaustive;
    let map_version = |version: &str| {
        version_spelling(
            version,
            if format == RepositoryFormat::Pypi {
                metadata.authoritative_versions.as_ref()
            } else {
                None
            },
            local,
        )
    };
    let mut known: BTreeSet<String> = metadata.versions.iter().map(|v| map_version(v)).collect();
    let mut entries = listing.unwrap_or_default();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let keys: BTreeSet<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
    let mut rows = Vec::new();
    let mut legacy = Vec::new();
    let mut legacy_npm = BTreeSet::new();
    for (key, meta) in &entries {
        let Some(filename) = key.strip_prefix(&prefix) else {
            continue;
        };
        let body = match format {
            RepositoryFormat::Docker => false,
            RepositoryFormat::Npm => filename.ends_with(".tgz"),
            RepositoryFormat::Pypi => {
                !filename.contains('/')
                    && nora_registry::pypi_files::served_extension(filename).is_some()
            }
        };
        if !body {
            continue;
        }
        let raw_version = match format {
            RepositoryFormat::Docker => None,
            RepositoryFormat::Npm => crate::curation::parse_npm_tarball_version(name, filename),
            RepositoryFormat::Pypi => nora_registry::pypi_files::parse_pypi_version(name, filename),
        };
        if let Some(version) = &raw_version {
            if format == RepositoryFormat::Pypi || legacy_npm.insert(version.clone()) {
                legacy.push(StoredVersion {
                    version: version.clone(),
                    size: meta.size,
                    updated: components::format_timestamp(meta.modified),
                });
            }
        }
        let version = raw_version.map(|v| map_version(&v));
        known.extend(version.iter().cloned());
        let mut row = empty_file(
            version,
            if advertisement_known {
                Some(false)
            } else {
                None
            },
        );
        row.filename = Some(filename.into());
        row.retained = "stored";
        row.stored_size = Some(meta.size);
        row.updated = Some(components::format_timestamp(meta.modified));
        row.key = Some(key.clone());
        row.sidecar = keys.contains(format!("{key}.sha256").as_str());
        rows.push(row);
    }
    let mut ads: BTreeMap<String, Vec<&crate::registry::saved::Distribution>> = BTreeMap::new();
    for file in &metadata.files {
        ads.entry(map_version(&file.version))
            .or_default()
            .push(file);
    }
    for (version, advertisements) in ads {
        if format == RepositoryFormat::Npm {
            let indices: Vec<_> = rows
                .iter()
                .enumerate()
                .filter_map(|(i, file)| {
                    (file.version.as_ref() == Some(&version) && file.key.is_some()).then_some(i)
                })
                .collect();
            let index = if let [index] = indices.as_slice() {
                *index
            } else {
                for &index in &indices {
                    rows[index].advertised = None;
                    rows[index].warnings.push(
                        "Saved advertisement cannot be assigned to one retained filename.".into(),
                    );
                }
                let mut row = empty_file(Some(version.clone()), Some(true));
                if stored_available && indices.is_empty() {
                    row.retained = "not_stored";
                }
                rows.push(row);
                rows.len() - 1
            };
            let row = &mut rows[index];
            row.advertised = Some(true);
            for advertisement in advertisements {
                row.checksums
                    .extend(advertisement.checksums.iter().cloned());
            }
        } else {
            for advertisement in advertisements {
                let index = rows
                    .iter()
                    .position(|file| file.filename == advertisement.filename)
                    .unwrap_or_else(|| {
                        let mut row = empty_file(Some(version.clone()), Some(true));
                        row.filename.clone_from(&advertisement.filename);
                        if stored_available {
                            row.retained = "not_stored";
                        }
                        rows.push(row);
                        rows.len() - 1
                    });
                let row = &mut rows[index];
                row.advertised = Some(true);
                row.advertised_size = advertisement.size;
                row.checksums
                    .extend(advertisement.checksums.iter().cloned());
            }
        }
    }
    known.retain(|version| {
        let valid = crate::repository_management::valid_version(version);
        if !valid {
            warnings.push(
                "Some recorded versions have unsafe coordinates and cannot be browsed.".into(),
            );
        }
        valid
    });
    let complete = stored_available && metadata_complete && warnings.is_empty();
    let selected_version = query.version.as_deref().map(map_version);
    if selected_version
        .as_ref()
        .is_some_and(|v| !known.contains(v))
        && complete
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut versions = Vec::new();
    for version in known {
        let files: Vec<_> = rows
            .iter()
            .filter(|file| file.version.as_ref() == Some(&version))
            .collect();
        let stored = files.iter().filter(|f| f.retained == "stored").count();
        let ambiguous = files
            .iter()
            .any(|f| f.advertised == Some(true) && f.retained == "unknown");
        versions.push(Version {
            filename_only: metadata
                .authoritative_versions
                .as_ref()
                .is_some_and(|v| !v.contains(&version)),
            version,
            stored_files: stored_available.then_some(stored),
            advertised_only_files: (stored_available
                && metadata_complete
                && !ambiguous
                && (advertisement_known || files.iter().any(|file| file.advertised == Some(true))))
            .then(|| {
                files
                    .iter()
                    .filter(|f| f.advertised == Some(true) && f.retained == "not_stored")
                    .count()
            }),
            stored_size: stored_available.then(|| files.iter().filter_map(|f| f.stored_size).sum()),
        });
    }
    versions.sort_by(|a, b| version_order(format, &a.version, &b.version));
    legacy.sort_by(|a, b| version_order(format, &a.version, &b.version));
    rows.sort_by(|a, b| a.filename.cmp(&b.filename).then(a.retained.cmp(b.retained)));
    let mut selected = Vec::new();
    let mut other = Vec::new();
    for file in rows {
        if file.version.is_none() {
            other.push(file);
        } else if selected_version.is_some() && file.version == selected_version {
            selected.push(file);
        }
    }
    let page = query.page.unwrap_or(1).max(1);
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let other = paginate(other, query.other_page.unwrap_or(1).max(1), limit);
    let other_files = if selected_version.is_none() {
        hydrate(state, other).await
    } else {
        Page {
            items: Vec::new(),
            ..other
        }
    };
    let files = if selected_version.is_some() {
        Some(hydrate(state, paginate(selected, page, limit)).await)
    } else {
        None
    };
    Ok(Inventory {
        complete,
        stored_available,
        metadata_complete,
        advertisements_enumerated: metadata.exhaustive,
        sources: metadata.sources,
        warnings,
        versions: paginate(
            versions,
            if selected_version.is_some() { 1 } else { page },
            limit,
        ),
        other_files,
        selected_version,
        files,
        legacy,
        description: metadata.description,
    })
}

#[cfg(test)]
mod tests;
