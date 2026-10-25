// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Snapshot inspection must not enter refresh or cache-proof migration paths.

use super::*;
use crate::registry::saved::{Checksum, Distribution, Metadata, Source, SourceState};

pub(crate) async fn read(state: &AppState, name: &str) -> Metadata {
    let mut result = Metadata::default();
    let mut source = Source {
        source: "Saved PyPI index".into(),
        state: SourceState::NotApplicable,
        saved_at: None,
        expired: None,
    };
    if named::is_proxy(state) {
        match named::stored(state, name).await {
            Ok(Some(snapshot)) => {
                source.saved_at = Some(snapshot.fetched_at);
                source.expired = crate::registry::transport::policy(state).map(|policy| {
                    !crate::cache_ttl::is_within_ttl(
                        snapshot.fetched_at,
                        i64::try_from(policy.metadata_ttl).unwrap_or(i64::MAX),
                    )
                });
                source.state = SourceState::Absent;
                result.exhaustive = true;
                if let Some(project) = &snapshot.project {
                    source.state = SourceState::Available;
                    result.authoritative_versions.clone_from(&project.versions);
                    result
                        .versions
                        .extend(project.versions.iter().flatten().cloned());
                    for file in &project.files {
                        let Some(version) =
                            nora_registry::pypi_files::parse_pypi_version(name, &file.filename)
                        else {
                            result.warn(
                                "Some saved distribution filenames have no recognized version.",
                            );
                            continue;
                        };
                        result.versions.insert(version.clone());
                        result.files.push(Distribution {
                            version,
                            filename: Some(file.filename.clone()),
                            size: file.size,
                            checksums: file
                                .hashes
                                .iter()
                                .map(|(algorithm, value)| Checksum {
                                    algorithm: algorithm.clone(),
                                    value: value.clone(),
                                    representation: "hex",
                                    source: source.source.clone(),
                                    saved_at: source.saved_at,
                                })
                                .collect(),
                        });
                    }
                }
            }
            Ok(None) => source.state = SourceState::Missing,
            Err(_) => {
                source.state = SourceState::Unavailable;
                result.warn("Saved PyPI metadata is unavailable or does not belong to the configured source.");
            }
        }
    }
    result.sources.push(source);
    result
}
