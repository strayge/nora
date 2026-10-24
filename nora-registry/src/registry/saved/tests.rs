// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use crate::{
    config::Config,
    test_helpers::{self, TestContext},
};
use serde_json::{json, Value};
use sha2::Digest;
use wiremock::MockServer;

async fn context(format: &str, kind: &str, upstream: &str) -> TestContext {
    test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|config| {
        let extra = if kind == "proxy" {
            format!("upstream='{upstream}'\nmetadata_ttl=0")
        } else {
            String::new()
        };
        config.repositories = toml::from_str::<Config>(&format!(
            "[repositories.test]\nformat='{format}'\nkind='{kind}'\npaths=['/test/']\n{extra}\n"
        ))
        .unwrap()
        .repositories;
    }))
    .await
}

async fn npm_record(context: &TestContext, key: &str, document: Value, upstream: &str) {
    let storage = &context.state.repositories.get("test").unwrap().storage;
    let bytes = serde_json::to_vec(&document).unwrap();
    storage.put(key, &bytes).await.unwrap();
    let record = format!(
        "{}\n{}",
        crate::circuit_breaker::upstream_key("npm", upstream),
        hex::encode(sha2::Sha256::digest(&bytes))
    );
    storage
        .put(&crate::registry::npm::origin_key(key), record.as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn local_version_documents_survive_without_an_assembled_packument() {
    let _lock = crate::test_env::env_lock();
    let context = context("npm", "local", "").await;
    let repository = context.state.repositories.get("test").unwrap();
    repository
        .storage
        .put(
            "npm/demo/versions/1.0.0.json",
            br#"{"dist":{"shasum":"abc"}}"#,
        )
        .await
        .unwrap();
    let state = repository.state(&context.state);
    let metadata = crate::registry::npm::saved::read(&state, "demo").await;
    assert!(metadata.complete());
    assert!(metadata.versions.contains("1.0.0"));
    assert_eq!(metadata.files[0].checksums[0].value, "abc");
    assert!(!repository
        .storage
        .exists("npm/demo/metadata.json")
        .await
        .unwrap());
}

#[tokio::test]
async fn proxy_manifests_keep_declared_versions_provenance_and_partial_errors_without_refresh() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let context = context("npm", "proxy", &upstream.uri()).await;
    for (key, document, origin) in [
        (
            "npm/demo/upstream/packument.json",
            json!({"name":"demo","versions":{"1.0.0":{"dist":{"integrity":"sha512-one"}}}}),
            upstream.uri(),
        ),
        (
            "npm/demo/upstream/manifests/latest.json",
            json!({"name":"demo","version":"2.0.0","dist":{"integrity":"sha512-two","tarball":"https://private.invalid/file?secret=hidden"}}),
            upstream.uri(),
        ),
        (
            "npm/demo/upstream/metadata.json",
            json!({"name":"demo","version":"3.0.0","dist":{"shasum":"three"}}),
            upstream.uri(),
        ),
        (
            "npm/demo/upstream/manifests/wrong.json",
            json!({"name":"other","version":"4.0.0","dist":{"shasum":"four"}}),
            upstream.uri(),
        ),
        (
            "npm/demo/upstream/manifests/untrusted.json",
            json!({"name":"demo","version":"5.0.0","dist":{"shasum":"five"}}),
            "https://wrong.invalid".into(),
        ),
    ] {
        npm_record(&context, key, document, &origin).await;
    }
    let repository = context.state.repositories.get("test").unwrap();
    let before = repository.storage.list_internal(".nora-").await.unwrap();
    let metadata =
        crate::registry::npm::saved::read(&repository.state(&context.state), "demo").await;
    assert_eq!(
        metadata.versions.into_iter().collect::<Vec<_>>(),
        ["1.0.0", "2.0.0", "3.0.0"]
    );
    assert!(!metadata.warnings.is_empty());
    assert_eq!(metadata.files.len(), 3);
    assert_eq!(
        before,
        repository.storage.list_internal(".nora-").await.unwrap()
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(!serde_json::to_string(&metadata.sources)
        .unwrap()
        .contains("secret"));
}

#[tokio::test]
async fn an_orphan_local_packument_does_not_assert_complete_absence() {
    let _lock = crate::test_env::env_lock();
    let context = context("npm", "local", "").await;
    let repository = context.state.repositories.get("test").unwrap();
    repository
        .storage
        .put(
            "npm/demo/metadata.json",
            br#"{"name":"demo","versions":{"1.0.0":{}}}"#,
        )
        .await
        .unwrap();
    let metadata =
        crate::registry::npm::saved::read(&repository.state(&context.state), "demo").await;
    assert!(!metadata.complete());
    assert!(metadata.versions.is_empty());
    assert!(repository
        .storage
        .exists("npm/demo/metadata.json")
        .await
        .unwrap());
}

#[tokio::test]
async fn quarantined_pypi_snapshots_are_observable_but_source_mismatches_are_not() {
    let _lock = crate::test_env::env_lock();
    let upstream = MockServer::start().await;
    let context = context("pypi", "proxy", &upstream.uri()).await;
    let repository = context.state.repositories.get("test").unwrap();
    let mut snapshot = json!({"schema":2,"name":"demo","source_id":repository.source_id,"fetched_at":0,"validators":{},"page_url":"https://private.invalid/?secret=hidden","content_type":"application/json","project":{"name":"demo","versions":["0.2"],"status":{"status":"quarantined"},"files":[{"filename":"demo-0.3.tar.gz","url":"https://private.invalid/file?secret=hidden","hashes":{"sha256":"abcd"},"size":12}]}});
    repository
        .storage
        .put(
            ".nora-pypi-index/demo.json",
            &serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .unwrap();
    let state = repository.state(&context.state);
    let metadata = crate::registry::pypi::saved::read(&state, "demo").await;
    assert!(metadata.complete());
    assert_eq!(metadata.files.len(), 1);
    assert!(metadata.versions.contains("0.2") && metadata.versions.contains("0.3"));
    assert_eq!(metadata.sources[0].expired, Some(true));
    snapshot["source_id"] = json!("wrong");
    repository
        .storage
        .put(
            ".nora-pypi-index/demo.json",
            &serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .unwrap();
    let metadata = crate::registry::pypi::saved::read(&state, "demo").await;
    assert!(!metadata.complete());
    assert!(metadata.files.is_empty());
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
