// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::repositories::RepositoryFormat;
use crate::repository_management::{version_for_key, Inventory, Operation};
use crate::storage::StorageError;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) async fn delete(op: &mut Operation) -> Result<(), StorageError> {
    let lock = op.state.publish_lock(&format!("pypi/{}/", op.name));
    let _guard = lock.lock().await;
    let inventory = Inventory::read(&op.state, RepositoryFormat::Pypi, &op.name).await?;
    let keys = inventory
        .keys
        .iter()
        .filter(|key| {
            op.version.as_deref().is_none_or(|version| {
                version_for_key(RepositoryFormat::Pypi, &op.name, key).as_deref() == Some(version)
            })
        })
        .cloned()
        .collect();
    remove_files(op, &inventory, keys).await?;
    let remaining = Inventory::read(&op.state, RepositoryFormat::Pypi, &op.name).await?;
    if remaining.files.is_empty() {
        remove_files(op, &remaining, remaining.keys.clone()).await?;
        op.result.package_removed = true;
    } else {
        op.result.residual_files = remaining
            .files
            .iter()
            .filter(|file| file.version.is_none())
            .count();
    }
    Ok(())
}

fn parent_filename<'a>(name: &str, key: &'a str) -> Option<&'a str> {
    let file = if let Some(rest) = key.strip_prefix(&format!("{READY_PREFIX}{name}/")) {
        rest.strip_suffix(".json")?
    } else if let Some(rest) = key.strip_prefix(&format!("{HASH_MARKER_PREFIX}{name}/")) {
        rest.split('/').next()?
    } else {
        key.strip_prefix(&format!("pypi/{name}/"))?
    };
    let file = file.strip_suffix(".sha256").unwrap_or(file);
    Some(file.strip_suffix(".metadata").unwrap_or(file))
}

async fn remove_files(
    op: &mut Operation,
    inventory: &Inventory,
    keys: BTreeSet<String>,
) -> Result<(), StorageError> {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for key in keys {
        let filename = parent_filename(&op.name, &key).unwrap_or("").to_owned();
        groups.entry(filename).or_default().insert(key);
    }
    for (filename, mut keys) in groups {
        let body = format!("pypi/{}/{filename}", op.name);
        let metadata = format!("{body}.metadata");
        let ready_lock = op
            .state
            .publish_lock(&cache::ready_key(&op.name, &filename));
        let _ready_guard = ready_lock.lock().await;
        let metadata_ready_lock = op
            .state
            .publish_lock(&cache::ready_key(&op.name, &format!("{filename}.metadata")));
        let _metadata_ready_guard = metadata_ready_lock.lock().await;
        let file_lock = op.state.publish_lock(&body);
        let _file_guard = file_lock.lock().await;
        let metadata_lock = op.state.publish_lock(&metadata);
        let _metadata_guard = metadata_lock.lock().await;
        // A queued backfill may have run after the initial inventory listing.
        for file in [&filename, &format!("{filename}.metadata")] {
            keys.extend(
                op.state
                    .storage
                    .list(&format!("{HASH_MARKER_PREFIX}{}/{file}/", op.name))
                    .await?,
            );
        }
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort_by_key(|key| {
            if key.starts_with(READY_PREFIX) {
                0
            } else if key == &body || key == &metadata {
                1
            } else {
                2
            }
        });
        for key in keys {
            op.delete(&key, inventory.body_size(&key)).await?;
        }
    }
    Ok(())
}
