// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::super::*;
use crate::{
    storage::{FileMeta, LocalStorage, Storage, StorageBackend, StorageError},
    test_helpers::{self, body_bytes, send, TestContext},
};
use axum::{
    body::{Body, Bytes},
    http::Method,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::Digest as _;
use std::{
    path::Path,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::io::AsyncRead;

struct Probe {
    inner: LocalStorage,
    calls: Mutex<Vec<(&'static str, String)>>,
    fail_list: Mutex<Option<&'static str>>,
    fail_get: Mutex<Option<&'static str>>,
    missing_body_pins: AtomicBool,
}

impl Probe {
    fn record(&self, operation: &'static str, key: &str) {
        self.calls.lock().push((operation, key.into()));
    }
    fn clear(&self) {
        self.calls.lock().clear();
    }
    fn assert_read_only(&self) {
        for (operation, key) in self.calls.lock().iter() {
            assert!(
                !matches!(*operation, "put" | "delete" | "copy" | "put_from_path"),
                "{operation}: {key}"
            );
            assert!(
                !matches!(*operation, "get" | "get_reader")
                    || nora_registry::pypi_files::served_extension(key).is_none(),
                "artifact body read: {key}"
            );
        }
    }
}

#[async_trait::async_trait]
impl StorageBackend for Probe {
    async fn put(&self, key: &str, data: &[u8], sha256: &str) -> crate::storage::Result<()> {
        self.record("put", key);
        self.inner.put(key, data, sha256).await
    }
    async fn get(&self, key: &str) -> crate::storage::Result<(Bytes, Option<String>)> {
        self.record("get", key);
        if self
            .fail_get
            .lock()
            .is_some_and(|suffix| key.ends_with(suffix))
        {
            return Err(StorageError::Network("injected read failure".into()));
        }
        self.inner.get(key).await
    }
    async fn pin(&self, key: &str) -> Option<String> {
        self.record("pin", key);
        if self.missing_body_pins.load(Ordering::SeqCst)
            && nora_registry::pypi_files::served_extension(key).is_some()
        {
            None
        } else {
            self.inner.pin(key).await
        }
    }
    async fn delete(&self, key: &str) -> crate::storage::Result<()> {
        self.record("delete", key);
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> crate::storage::Result<Vec<String>> {
        self.record("list", prefix);
        if self
            .fail_list
            .lock()
            .is_some_and(|suffix| prefix.ends_with(suffix))
        {
            Err(StorageError::Network("injected listing failure".into()))
        } else {
            self.inner.list(prefix).await
        }
    }
    async fn list_with_meta(
        &self,
        prefix: &str,
    ) -> crate::storage::Result<Vec<(String, FileMeta)>> {
        self.record("list_with_meta", prefix);
        if self
            .fail_list
            .lock()
            .is_some_and(|suffix| prefix.ends_with(suffix))
        {
            Err(StorageError::Network("injected listing failure".into()))
        } else {
            self.inner.list_with_meta(prefix).await
        }
    }
    async fn stat(&self, key: &str) -> Option<FileMeta> {
        self.record("stat", key);
        self.inner.stat(key).await
    }
    async fn exists(&self, key: &str) -> crate::storage::Result<bool> {
        self.record("exists", key);
        self.inner.exists(key).await
    }
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
    async fn total_size(&self) -> u64 {
        self.inner.total_size().await
    }
    fn backend_name(&self) -> &'static str {
        "recording"
    }
    async fn put_from_path(
        &self,
        key: &str,
        src: &Path,
        sha256: Option<&str>,
    ) -> crate::storage::Result<()> {
        self.record("put_from_path", key);
        self.inner.put_from_path(key, src, sha256).await
    }
    async fn copy(&self, src: &str, dst: &str, sha256: Option<&str>) -> crate::storage::Result<()> {
        self.record("copy", dst);
        self.inner.copy(src, dst, sha256).await
    }
    async fn get_reader(
        &self,
        key: &str,
    ) -> crate::storage::Result<(u64, Option<String>, Pin<Box<dyn AsyncRead + Send + Unpin>>)> {
        self.record("get_reader", key);
        self.inner.get_reader(key).await
    }
}

async fn context(
    format: &str,
    proxy: bool,
    upstream: &str,
) -> (TestContext, Arc<Probe>, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let probe = Arc::new(Probe {
        inner: LocalStorage::new(root.path().to_str().unwrap()),
        calls: Mutex::new(Vec::new()),
        fail_list: Mutex::new(None),
        fail_get: Mutex::new(None),
        missing_body_pins: AtomicBool::new(false),
    });
    let storage = Storage::from_backend(probe.clone());
    let kind = if proxy { "proxy" } else { "local" };
    let extra = if proxy {
        format!("upstream='{upstream}'\nmetadata_ttl=300")
    } else {
        String::new()
    };
    let context = test_helpers::with_named_repositories(test_helpers::create_test_context_with_storage_and_config(storage, |config| {
        config.repositories = toml::from_str::<crate::config::Config>(&format!("[repositories.test]\nformat='{format}'\nkind='{kind}'\npaths=['/test/']\n{extra}\n[repositories.other]\nformat='{format}'\nkind='local'\npaths=['/other/']\n[repositories.all]\nformat='{format}'\nkind='group'\npaths=['/all/']\nmembers=['test','other']")).unwrap().repositories;
    })).await;
    (context, probe, root)
}

async fn json_response(context: &TestContext, query: &str) -> Value {
    let response = send(
        &context.app,
        Method::GET,
        &format!("/api/ui/repositories/test/package/demo{query}"),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

async fn npm_record(context: &TestContext, key: &str, document: Value, upstream: &str) {
    let storage = &context.state.repositories.get("test").unwrap().storage;
    let data = serde_json::to_vec(&document).unwrap();
    storage.put(key, &data).await.unwrap();
    let record = format!(
        "{}\n{}",
        crate::circuit_breaker::upstream_key("npm", upstream),
        hex::encode(sha2::Sha256::digest(&data))
    );
    storage
        .put(&crate::registry::npm::origin_key(key), record.as_bytes())
        .await
        .unwrap();
}

async fn pypi_snapshot(context: &TestContext, project: Value, source: Option<&str>) {
    let member = context.state.repositories.get("test").unwrap();
    let data = json!({"schema":2,"name":"demo","source_id":source.unwrap_or(&member.source_id),"fetched_at":0,"validators":{},"page_url":"https://private.invalid/?secret=hidden","content_type":"application/json","project":project});
    member
        .storage
        .put(
            ".nora-pypi-index/demo.json",
            &serde_json::to_vec(&data).unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn browsing_reads_only_visible_checksums_and_never_reads_bodies_or_writes() {
    let _lock = crate::test_env::env_lock();
    let (context, probe, _root) = context("pypi", false, "").await;
    let storage = &context.state.repositories.get("test").unwrap().storage;
    for file in [
        "demo-1.0-py3-none-any.whl",
        "demo-1.0.tar.gz",
        "demo-2.0.tar.gz",
        "orphan.whl",
    ] {
        storage
            .put(&format!("pypi/demo/{file}"), b"body")
            .await
            .unwrap();
        storage
            .put(
                &format!("pypi/demo/{file}.sha256"),
                hex::encode(sha2::Sha256::digest(b"body")).as_bytes(),
            )
            .await
            .unwrap();
    }
    probe.clear();
    let detail = json_response(&context, "?version=1.0&limit=1").await;
    assert_eq!(
        detail["artifacts"]["files"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    probe.assert_read_only();
    let body_pins: Vec<_> = probe
        .calls
        .lock()
        .iter()
        .filter(|(op, key)| {
            *op == "pin" && nora_registry::pypi_files::served_extension(key).is_some()
        })
        .cloned()
        .collect();
    assert_eq!(body_pins.len(), 1);
    assert!(body_pins[0].1.ends_with("demo-1.0-py3-none-any.whl"));
    let sidecar_reads: Vec<_> = probe
        .calls
        .lock()
        .iter()
        .filter(|(op, key)| *op == "get" && key.ends_with(".sha256"))
        .cloned()
        .collect();
    assert_eq!(sidecar_reads.len(), 1);
    assert!(sidecar_reads[0]
        .1
        .ends_with("demo-1.0-py3-none-any.whl.sha256"));
    probe.clear();
    let overview = json_response(&context, "?limit=1").await;
    assert_eq!(overview["artifacts"]["other_files"]["total"], 1);
    probe.assert_read_only();
    let body_pins: Vec<_> = probe
        .calls
        .lock()
        .iter()
        .filter(|(op, key)| {
            *op == "pin" && nora_registry::pypi_files::served_extension(key).is_some()
        })
        .cloned()
        .collect();
    assert_eq!(body_pins.len(), 1);
    assert!(body_pins[0].1.ends_with("orphan.whl"));
    let sidecar_reads: Vec<_> = probe
        .calls
        .lock()
        .iter()
        .filter(|(op, key)| *op == "get" && key.ends_with(".sha256"))
        .cloned()
        .collect();
    assert_eq!(sidecar_reads.len(), 1);
    assert!(sidecar_reads[0].1.ends_with("orphan.whl.sha256"));
    *probe.fail_get.lock() = Some("demo-1.0-py3-none-any.whl.sha256");
    probe.missing_body_pins.store(true, Ordering::SeqCst);
    probe.clear();
    let missing = json_response(&context, "?version=1.0&limit=1").await;
    assert_eq!(
        missing["artifacts"]["files"]["items"][0]["retained"],
        "stored"
    );
    assert!(missing["artifacts"]["files"]["items"][0]["checksums"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!missing["artifacts"]["files"]["items"][0]["warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    probe.assert_read_only();
}

#[tokio::test]
async fn pypi_partial_reads_preserve_unknown_retention_authoritative_versions_and_group_failures() {
    let _lock = crate::test_env::env_lock();
    let upstream = wiremock::MockServer::start().await;
    let (context, probe, _root) = context("pypi", true, &upstream.uri()).await;
    let storage = &context.state.repositories.get("test").unwrap().storage;
    storage
        .put("pypi/demo/demo-0.3.tar.gz", b"body")
        .await
        .unwrap();
    pypi_snapshot(&context, json!({"name":"demo","versions":["0.2"],"status":{"status":"quarantined"},"files":[{"filename":"demo-0.3.tar.gz","url":"https://private.invalid/?secret=hidden","hashes":{"sha256":hex::encode(sha2::Sha256::digest(b"body"))},"size":4}]}), None).await;
    *probe.fail_list.lock() = Some("pypi/demo/");
    probe.clear();
    let partial = json_response(&context, "?version=0.3").await;
    assert_eq!(partial["artifacts"]["complete"], false);
    assert!(partial["artifacts"]["versions"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["version"] == "0.2"));
    assert_eq!(
        partial["artifacts"]["files"]["items"][0]["retained"],
        "unknown"
    );
    assert!(partial["artifacts"]["versions"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v["stored_files"].is_null()));
    assert!(!partial.to_string().contains("secret"));
    probe.assert_read_only();
    *probe.fail_list.lock() = None;
    pypi_snapshot(&context, Value::Null, None).await;
    probe.clear();
    let absence = json_response(&context, "?version=0.3").await;
    assert_eq!(
        absence["artifacts"]["files"]["items"][0]["advertised"],
        false
    );
    probe.assert_read_only();
    let other = context.state.repositories.get("other").unwrap();
    other
        .storage
        .put("pypi/demo/demo-9.0.tar.gz", b"other")
        .await
        .unwrap();
    probe.clear();
    let group = send(
        &context.app,
        Method::GET,
        "/api/ui/repositories/all/package/demo",
        Body::empty(),
    )
    .await;
    assert_eq!(group.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!probe
        .calls
        .lock()
        .iter()
        .any(|(_, key)| key.contains("repositories/other/")));
    probe.assert_read_only();
    pypi_snapshot(&context, Value::Null, Some("wrong-source")).await;
    probe.clear();
    let untrusted = json_response(&context, "?version=0.3").await;
    assert_eq!(untrusted["artifacts"]["metadata_complete"], false);
    assert_eq!(
        untrusted["artifacts"]["files"]["items"][0]["retained"],
        "stored"
    );
    probe.assert_read_only();
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn npm_conflicts_and_rejected_source_records_do_not_leak_through_compatibility_fields() {
    let _lock = crate::test_env::env_lock();
    let upstream = wiremock::MockServer::start().await;
    let (context, probe, _root) = context("npm", true, &upstream.uri()).await;
    let storage = &context.state.repositories.get("test").unwrap().storage;
    storage
        .put("npm/demo/tarballs/demo-1.0.0.tgz", b"body")
        .await
        .unwrap();
    let actual = format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(b"body"))
    );
    let different = format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode([0; 32])
    );
    npm_record(
        &context,
        "npm/demo/upstream/packument.json",
        json!({"name":"demo","versions":{"1.0.0":{"dist":{"integrity":actual}}}}),
        &upstream.uri(),
    )
    .await;
    npm_record(&context, "npm/demo/upstream/manifests/latest.json", json!({"name":"demo","version":"1.0.0","dist":{"integrity":different,"tarball":"https://private.invalid/?secret=hidden"}}), &upstream.uri()).await;
    probe.clear();
    let conflict = json_response(&context, "?version=1.0.0").await;
    assert_eq!(
        conflict["artifacts"]["files"]["items"][0]["checksum_conflict"],
        true
    );
    assert_eq!(
        conflict["artifacts"]["files"]["items"][0]["checksums"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(!conflict.to_string().contains("secret"));
    probe.assert_read_only();
    npm_record(&context, "npm/demo/upstream/packument.json", json!({"name":"demo","description":"REJECTED PRIVATE METADATA","versions":{"9.0.0":{"dist":{"integrity":actual}}}}), "https://wrong.invalid").await;
    probe.clear();
    let rejected = json_response(&context, "").await;
    assert!(rejected["description"].is_null());
    assert!(!rejected.to_string().contains("REJECTED PRIVATE"));
    assert_eq!(rejected["stored_versions"][0]["version"], "1.0.0");
    assert!(!rejected["artifacts"]["versions"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["version"] == "9.0.0"));
    assert_eq!(rejected["artifacts"]["complete"], false);
    probe.assert_read_only();
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn total_read_failure_is_an_error_but_local_overview_keeps_recovery_controls() {
    let _lock = crate::test_env::env_lock();
    let (context, probe, _root) = context("pypi", false, "").await;
    *probe.fail_list.lock() = Some("pypi/demo/");
    probe.clear();
    let overview = send(
        &context.app,
        Method::GET,
        "/ui/repositories/test/package/demo?retry_version=1.0",
        Body::empty(),
    )
    .await;
    assert_eq!(overview.status(), StatusCode::OK);
    let html = String::from_utf8(body_bytes(overview).await.to_vec()).unwrap();
    assert!(html.contains("Inventory is incomplete") && html.contains("Retry version deletion"));
    let detail = send(
        &context.app,
        Method::GET,
        "/api/ui/repositories/test/package/demo?version=1.0",
        Body::empty(),
    )
    .await;
    assert_eq!(detail.status(), StatusCode::SERVICE_UNAVAILABLE);
    probe.assert_read_only();
}
