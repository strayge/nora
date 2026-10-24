// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::repositories::RepositoryFormat;
use crate::repository_management::{version_for_key, Inventory, Operation};
use crate::storage::StorageError;
use std::collections::BTreeSet;

pub(crate) async fn delete(op: &mut Operation) -> Result<(), StorageError> {
    let lock = op.state.publish_lock(&published_packument_key(&op.name));
    let _guard = lock.lock().await;
    let inventory = Inventory::read(&op.state, RepositoryFormat::Npm, &op.name).await?;
    let Some(version) = op.version.clone() else {
        return whole_package(op, &inventory).await;
    };
    let version_key = format!("npm/{}/versions/{version}.json", op.name);
    let mut selected: BTreeSet<_> = inventory
        .keys
        .iter()
        .filter(|key| {
            **key == version_key
                || version_for_key(RepositoryFormat::Npm, &op.name, key).as_deref()
                    == Some(&version)
        })
        .cloned()
        .collect();
    let tag_prefix = format!("npm/{}/dist-tags/", op.name);
    // Read tag pointers before the first deletion, never treating read failure as absence.
    for key in inventory
        .keys
        .iter()
        .filter(|key| key.starts_with(&tag_prefix))
    {
        let bytes = op.state.storage.get(key).await?;
        let value = std::str::from_utf8(&bytes).map_err(invalid_metadata)?;
        if value.trim() == version {
            selected.insert(key.clone());
        }
    }
    let mut keys: Vec<_> = selected.into_iter().collect();
    keys.sort_by_key(|key| {
        if inventory.body_size(key).is_some() {
            0
        } else if key == &version_key {
            2
        } else if key.starts_with(&tag_prefix) {
            3
        } else {
            1
        }
    });
    for key in keys {
        op.delete(&key, inventory.body_size(&key)).await?;
    }
    let remaining = Inventory::read(&op.state, RepositoryFormat::Npm, &op.name).await?;
    if remaining.releases.is_empty() {
        whole_package(op, &remaining).await?;
    } else if remaining
        .keys
        .iter()
        .any(|key| key.starts_with(&format!("npm/{}/versions/", op.name)))
    {
        strict_rebuild(op).await?;
    } else {
        let metadata = published_packument_key(&op.name);
        op.delete(&metadata, None).await?;
        op.delete(&origin_key(&metadata), None).await?;
    }
    if !op.result.package_removed {
        op.result.residual_files = remaining.unassigned.len();
    }
    Ok(())
}

async fn whole_package(op: &mut Operation, inventory: &Inventory) -> Result<(), StorageError> {
    let metadata = published_packument_key(&op.name);
    let mut keys: Vec<_> = inventory.keys.iter().collect();
    keys.sort_by_key(|key| {
        if inventory.body_size(key).is_some() || inventory.unassigned.contains(*key) {
            0
        } else if **key == metadata || **key == origin_key(&metadata) {
            2
        } else {
            1
        }
    });
    for key in keys {
        op.delete(key, inventory.body_size(key)).await?;
    }
    op.result.package_removed = true;
    Ok(())
}

fn invalid_metadata(error: impl std::fmt::Display) -> StorageError {
    StorageError::Network(format!("invalid surviving npm metadata: {error}"))
}

/// Deletion must not report success after silently dropping an unreadable survivor.
async fn strict_rebuild(op: &mut Operation) -> Result<(), StorageError> {
    let storage = &op.state.storage;
    let mut versions = serde_json::Map::new();
    let prefix = format!("npm/{}/versions/", op.name);
    for key in storage.list(&prefix).await? {
        let Some(version) = key
            .strip_prefix(&prefix)
            .and_then(|key| key.strip_suffix(".json"))
        else {
            return Err(invalid_metadata("invalid version document key"));
        };
        let document: serde_json::Value =
            serde_json::from_slice(&storage.get(&key).await?).map_err(invalid_metadata)?;
        if !document.is_object() {
            return Err(invalid_metadata("version is not an object"));
        }
        versions.insert(version.to_owned(), document);
    }
    let mut tags = serde_json::Map::new();
    let prefix = format!("npm/{}/dist-tags/", op.name);
    for key in storage.list(&prefix).await? {
        let value = storage.get(&key).await?;
        let value = std::str::from_utf8(&value)
            .map_err(invalid_metadata)?
            .trim();
        let tag = key
            .strip_prefix(&prefix)
            .ok_or_else(|| invalid_metadata("tag key"))?;
        tags.insert(tag.to_owned(), serde_json::Value::String(value.to_owned()));
    }
    if !tags.contains_key("latest") {
        if let Some(version) = max_semver(versions.keys()) {
            tags.insert("latest".into(), serde_json::Value::String(version));
        }
    }
    let mut document: serde_json::Value =
        match storage.get(&format!("npm/{}/pkg.json", op.name)).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_metadata)?,
            Err(StorageError::NotFound) => serde_json::json!({}),
            Err(error) => return Err(error),
        };
    let object = document
        .as_object_mut()
        .ok_or_else(|| invalid_metadata("package is not an object"))?;
    object.insert("name".into(), serde_json::Value::String(op.name.clone()));
    object.insert("versions".into(), versions.into());
    object.insert("dist-tags".into(), tags.into());
    let bytes = serde_json::to_vec(&document).map_err(invalid_metadata)?;
    if op.state.background_cancel.is_cancelled() {
        return Err(invalid_metadata("shutdown interrupted reconciliation"));
    }
    op.started = true;
    let key = published_packument_key(&op.name);
    storage.put(&key, &bytes).await?;
    if op.state.background_cancel.is_cancelled() {
        return Err(invalid_metadata("shutdown interrupted reconciliation"));
    }
    storage
        .put(
            &origin_key(&key),
            provenance_record(ASSEMBLED_BY_NORA, &bytes).as_bytes(),
        )
        .await?;
    Ok(())
}
