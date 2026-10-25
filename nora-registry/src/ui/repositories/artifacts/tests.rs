// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::{config::Config, test_helpers};

mod safety;

async fn context(format: &str) -> test_helpers::TestContext {
    test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|config| {
        config.repositories = toml::from_str::<Config>(&format!(
            "[repositories.test]\nformat='{format}'\nkind='local'\npaths=['/test/']"
        ))
        .unwrap()
        .repositories;
    }))
    .await
}

#[tokio::test]
async fn version_pages_preserve_exact_local_releases_and_page_checksum_evidence() {
    let _lock = crate::test_env::env_lock();
    let context = context("pypi").await;
    let member = context.state.repositories.get("test").unwrap();
    for filename in [
        "demo-1.0-py3-none-any.whl",
        "demo-1.0.tar.gz",
        "demo-1.0.0.tar.gz",
        "unassigned.whl",
    ] {
        member
            .storage
            .put(&format!("pypi/demo/{filename}"), b"body")
            .await
            .unwrap();
    }
    member
        .storage
        .put(
            "pypi/demo/demo-1.0-py3-none-any.whl.sha256",
            "00".repeat(32).as_bytes(),
        )
        .await
        .unwrap();
    let query = PackageQuery {
        version: Some("1.0".into()),
        limit: Some(1),
        ..Default::default()
    };
    let inventory = read(
        &member.state(&context.state),
        RepositoryFormat::Pypi,
        "demo",
        &query,
    )
    .await
    .unwrap();
    assert_eq!(inventory.legacy.len(), 3);
    assert_eq!(inventory.versions.total, 2);
    assert_eq!(inventory.other_files.total, 1);
    let files = inventory.files.unwrap();
    assert_eq!(files.total, 2);
    assert_eq!(files.items.len(), 1);
    assert_eq!(
        files.items[0].filename.as_deref(),
        Some("demo-1.0-py3-none-any.whl")
    );
    assert!(files.items[0].checksum_conflict);
    assert_eq!(files.items[0].checksums[0].source, "Storage pin");
    assert_eq!(files.items[0].advertised, None);
    assert_eq!(inventory.versions.items[0].advertised_only_files, None);
}

#[tokio::test]
async fn multiple_npm_layouts_do_not_duplicate_advertisements_or_claim_retention() {
    let _lock = crate::test_env::env_lock();
    let context = context("npm").await;
    let member = context.state.repositories.get("test").unwrap();
    for key in [
        "npm/demo/tarballs/demo-1.0.0.tgz",
        "npm/demo/tarballs/1.0.0/demo-1.0.0.tgz",
    ] {
        member.storage.put(key, b"body").await.unwrap();
    }
    member
        .storage
        .put(
            "npm/demo/versions/1.0.0.json",
            br#"{"dist":{"tarball":"https://private.invalid/?secret=hidden"}}"#,
        )
        .await
        .unwrap();
    member
        .storage
        .put("npm/demo/versions/2.0.0.json", b"{}")
        .await
        .unwrap();
    let query = PackageQuery {
        version: Some("1.0.0".into()),
        ..Default::default()
    };
    let inventory = read(
        &member.state(&context.state),
        RepositoryFormat::Npm,
        "demo",
        &query,
    )
    .await
    .unwrap();
    assert_eq!(inventory.legacy.len(), 1);
    let version = inventory
        .versions
        .items
        .iter()
        .find(|v| v.version == "1.0.0")
        .unwrap();
    assert_eq!(version.stored_files, Some(2));
    assert_eq!(version.advertised_only_files, None);
    let files = inventory.files.unwrap();
    assert_eq!(files.items.len(), 3);
    assert!(files
        .items
        .iter()
        .filter(|f| f.retained == "stored")
        .all(|f| f.advertised.is_none()));
    assert_eq!(
        files
            .items
            .iter()
            .filter(|f| f.retained == "unknown")
            .count(),
        1
    );
    assert!(!serde_json::to_string(&files).unwrap().contains("secret"));
    let empty = read(
        &member.state(&context.state),
        RepositoryFormat::Npm,
        "demo",
        &PackageQuery {
            version: Some("2.0.0".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(empty.files.unwrap().total, 0);
}

#[tokio::test]
async fn a_saved_manifest_cannot_assert_absence_for_another_stored_version() {
    use sha2::Digest as _;
    let _lock = crate::test_env::env_lock();
    let upstream = wiremock::MockServer::start().await;
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_config(|config| {
            config.repositories = toml::from_str::<Config>(&format!(
                "[repositories.test]\nformat='npm'\nkind='proxy'\npaths=['/test/']\nupstream='{}'",
                upstream.uri()
            ))
            .unwrap()
            .repositories;
        }),
    )
    .await;
    let member = context.state.repositories.get("test").unwrap();
    member
        .storage
        .put("npm/demo/tarballs/demo-1.0.0.tgz", b"body")
        .await
        .unwrap();
    let key = "npm/demo/upstream/manifests/latest.json";
    let data =
        br#"{"name":"demo","version":"2.0.0","dist":{"tarball":"https://private.invalid/file"}}"#;
    member.storage.put(key, data).await.unwrap();
    let provenance = format!(
        "{}\n{}",
        crate::circuit_breaker::upstream_key("npm", &upstream.uri()),
        hex::encode(sha2::Sha256::digest(data))
    );
    member
        .storage
        .put(
            &crate::registry::npm::origin_key(key),
            provenance.as_bytes(),
        )
        .await
        .unwrap();
    let state = member.state(&context.state);
    let inventory = read(
        &state,
        RepositoryFormat::Npm,
        "demo",
        &PackageQuery {
            version: Some("1.0.0".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!inventory.advertisements_enumerated);
    assert_eq!(inventory.files.unwrap().items[0].advertised, None);
    assert_eq!(
        inventory
            .versions
            .items
            .iter()
            .find(|v| v.version == "1.0.0")
            .unwrap()
            .advertised_only_files,
        None
    );
    let advertised = read(
        &state,
        RepositoryFormat::Npm,
        "demo",
        &PackageQuery {
            version: Some("2.0.0".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(advertised.files.unwrap().items[0].advertised, Some(true));
    assert_eq!(
        advertised
            .versions
            .items
            .iter()
            .find(|v| v.version == "2.0.0")
            .unwrap()
            .advertised_only_files,
        Some(1)
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[test]
fn legacy_sha1_integrity_is_a_recorded_checksum_too() {
    let checksum = Checksum {
        algorithm: "SRI".into(),
        value: format!(
            "sha1-{}",
            base64::engine::general_purpose::STANDARD.encode([7; 20])
        ),
        representation: "sri",
        source: "index".into(),
        saved_at: None,
    };
    assert!(digest_values(&checksum).contains_key("sha1"));
}

#[test]
fn sri_alternatives_and_equivalent_encodings_are_not_checksum_conflicts() {
    let hex = Checksum {
        algorithm: "sha256".into(),
        value: "01".repeat(32),
        representation: "hex",
        source: "pin".into(),
        saved_at: None,
    };
    let sri = Checksum {
        algorithm: "SRI".into(),
        value: format!(
            "sha256-{} sha256-{}",
            base64::engine::general_purpose::STANDARD.encode([1; 32]),
            base64::engine::general_purpose::STANDARD.encode([2; 32])
        ),
        representation: "sri",
        source: "index".into(),
        saved_at: None,
    };
    assert!(!digest_values(&hex)["sha256"].is_disjoint(&digest_values(&sri)["sha256"]));
}
