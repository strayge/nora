// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use super::*;
use crate::test_helpers::create_test_context;

fn config() -> Config {
    toml::from_str(
        r#"
[repositories.local]
format = "pypi"
kind = "local"
paths = ["/local/"]
[repositories.remote]
format = "pypi"
kind = "proxy"
paths = ["/remote/", "/remote-alias/"]
upstream = "https://one.example/simple/"
[repositories.all]
format = "pypi"
kind = "group"
paths = ["/all/"]
members = ["local", "remote"]
"#,
    )
    .unwrap()
}

#[tokio::test]
async fn namespaces_survive_restart_but_not_source_or_kind_reinterpretation() {
    let dir = tempfile::tempdir().unwrap();
    let root = Storage::new_local(dir.path().to_str().unwrap());
    let mut config = config();
    root.put("pypi/pkg/pkg-1.tar.gz", b"legacy").await.unwrap();
    let catalogue = RepositoryCatalogue::open(&config, &root).await.unwrap();
    assert_eq!(catalogue.members().count(), 2);
    assert!(catalogue.get("all").is_none());
    let local = catalogue.get("local").unwrap();
    let remote = catalogue.get("remote").unwrap();
    let key = "pypi/pkg/pkg-1.tar.gz";
    let (a, b) = tokio::join!(
        local.storage.put(key, b"published"),
        remote.storage.put(key, b"cached")
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(local.storage.get(key).await.unwrap().as_ref(), b"published");
    assert_eq!(remote.storage.get(key).await.unwrap().as_ref(), b"cached");
    assert_eq!(root.get(key).await.unwrap().as_ref(), b"legacy");
    let reopened =
        RepositoryCatalogue::open(&config, &Storage::new_local(dir.path().to_str().unwrap()))
            .await
            .unwrap();
    assert_eq!(
        reopened
            .get("remote")
            .unwrap()
            .storage
            .get(key)
            .await
            .unwrap()
            .as_ref(),
        b"cached"
    );
    let RepositoryConfig::Proxy(proxy) = config.repositories.get_mut("remote").unwrap() else {
        panic!()
    };
    proxy.paths = vec!["/renamed-alias/".into()];
    assert_eq!(
        RepositoryCatalogue::open(&config, &root)
            .await
            .unwrap()
            .get("remote")
            .unwrap()
            .storage
            .get(key)
            .await
            .unwrap()
            .as_ref(),
        b"cached"
    );
    let RepositoryConfig::Proxy(proxy) = config.repositories.get_mut("remote").unwrap() else {
        panic!()
    };
    proxy.upstream = "https://two.example/simple/".into();
    assert!(RepositoryCatalogue::open(&config, &root)
        .await
        .unwrap()
        .get("remote")
        .unwrap()
        .storage
        .list("")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(remote.storage.get(key).await.unwrap().as_ref(), b"cached");
    let RepositoryConfig::Proxy(proxy) = config.repositories.get_mut("remote").unwrap() else {
        panic!()
    };
    proxy.format = RepositoryFormat::Npm;
    assert!(RepositoryCatalogue::open(&config, &root)
        .await
        .err()
        .unwrap()
        .contains("stored format/kind differs"));
}

#[tokio::test]
async fn stored_identity_is_verified_and_unknown_namespaces_are_not_adopted() {
    let root = Storage::from_backend(Arc::new(crate::storage::ObjectStorage::in_memory()));
    root.put(
        "repositories/local/sources/local/pypi/pkg/pkg-1.tar.gz",
        b"unknown",
    )
    .await
    .unwrap();
    assert!(RepositoryCatalogue::open(&config(), &root)
        .await
        .err()
        .unwrap()
        .contains("no trusted identity"));
    root.delete("repositories/local/sources/local/pypi/pkg/pkg-1.tar.gz")
        .await
        .unwrap();
    root.put("repositories/local/.nora-repository.json", b"invalid")
        .await
        .unwrap();
    assert!(RepositoryCatalogue::open(&config(), &root)
        .await
        .err()
        .unwrap()
        .contains("invalid stored identity"));
}

#[tokio::test]
async fn alias_views_share_member_state_without_sharing_other_members_work() {
    let root = create_test_context();
    let mut cfg = (*root.state.config).clone();
    cfg.repositories = config().repositories;
    let catalogue = RepositoryCatalogue::open(&cfg, &root.state.storage)
        .await
        .unwrap();
    let first = catalogue.get("remote").unwrap().state(&root.state);
    let alias = catalogue.get("remote").unwrap().state(&root.state);
    let other = catalogue.get("local").unwrap().state(&root.state);
    assert!(Arc::ptr_eq(&first.publish_locks, &alias.publish_locks));
    assert!(!Arc::ptr_eq(&first.publish_locks, &other.publish_locks));
    assert!(Arc::ptr_eq(&first.repo_index, &alias.repo_index));
    assert!(!Arc::ptr_eq(&first.repo_index, &other.repo_index));
    assert!(Arc::ptr_eq(&first.reloadable, &root.state.reloadable));
    assert!(Arc::ptr_eq(&first.digest_store, &root.state.digest_store));
    assert_eq!(first.config.server, root.state.config.server);
    assert_eq!(
        serde_json::to_value(&first.config.auth).unwrap(),
        serde_json::to_value(&root.state.config.auth).unwrap()
    );
    assert_eq!(first.enabled_registries, root.state.enabled_registries);
    assert_eq!(first.config.pypi.upstreams().len(), 1);
    assert!(other.config.pypi.upstreams().is_empty());
    let key = "pypi/pkg/file";
    let (a, b) = tokio::join!(
        first
            .proxy_coalesce
            .coalesced(key, "pypi", std::time::Duration::from_secs(1), || async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                Some(Bytes::from_static(b"a"))
            }),
        other
            .proxy_coalesce
            .coalesced(key, "pypi", std::time::Duration::from_secs(1), || async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                Some(Bytes::from_static(b"b"))
            }),
    );
    assert_eq!(a.unwrap().as_ref(), b"a");
    assert_eq!(b.unwrap().as_ref(), b"b");
    first.cancel_token.cancel();
    assert!(root.state.cancel_token.is_cancelled());
}

#[tokio::test]
async fn root_and_member_maintenance_never_sweep_sibling_namespaces() {
    let root = create_test_context();
    let catalogue = RepositoryCatalogue::open(&config(), &root.state.storage)
        .await
        .unwrap();
    let a = catalogue.get("local").unwrap().state(&root.state);
    let b = catalogue.get("remote").unwrap().state(&root.state);
    let orphan = "pypi/pkg/pkg-1.tar.gz.sha256";
    a.storage.put(orphan, b"hash").await.unwrap();
    b.storage.put(orphan, b"hash").await.unwrap();
    crate::gc::run_gc(
        &root.state.storage,
        &root.state.publish_locks,
        false,
        0,
        false,
        0,
    )
    .await;
    assert!(a.storage.stat(orphan).await.is_some());
    assert!(b.storage.stat(orphan).await.is_some());
    crate::gc::run_gc(&a.storage, &a.publish_locks, false, 0, false, 0).await;
    assert!(a.storage.stat(orphan).await.is_none());
    assert!(b.storage.stat(orphan).await.is_some());
    let filename = "pypi/pkg/pkg-1.tar.gz";
    a.storage.put(filename, b"first").await.unwrap();
    b.storage.put(filename, b"second").await.unwrap();
    root.state.storage.put(filename, b"legacy").await.unwrap();
    let rules = vec![crate::config::RetentionRule {
        registry: "pypi".into(),
        name_glob: None,
        keep_last: Some(0),
        older_than_days: None,
        include_tags: Vec::new(),
        exclude_tags: Vec::new(),
    }];
    crate::retention::run_retention(&a.storage, &a.publish_locks, None, &rules, false).await;
    assert!(a.storage.stat(filename).await.is_none());
    assert_eq!(b.storage.get(filename).await.unwrap().as_ref(), b"second");
    assert_eq!(
        root.state.storage.get(filename).await.unwrap().as_ref(),
        b"legacy"
    );
}

#[tokio::test]
async fn backup_round_trip_preserves_hidden_identity_provenance_and_pins() {
    let source = tempfile::tempdir().unwrap();
    let root = Storage::new_local(source.path().to_str().unwrap());
    let catalogue = RepositoryCatalogue::open(&config(), &root).await.unwrap();
    let member = catalogue.get("remote").unwrap();
    let file = "pypi/pkg/pkg-1.tar.gz";
    let record = ".nora-origins/pypi/pkg/pkg-1.tar.gz";
    member.storage.put(file, b"retained").await.unwrap();
    member
        .storage
        .put(record, b"source-specific record")
        .await
        .unwrap();
    let extra=[(format!("{file}.metadata"),b"Metadata-Version: 2.3\n".to_vec()),
        (crate::registry::pypi_ready_key_for_body(file).unwrap(),serde_json::to_vec(&serde_json::json!({"source_id":member.source_id,"size":8,"hashes":{"sha256":"proof"}})).unwrap()),
        (crate::registry::pypi_ready_key_for_body(&format!("{file}.metadata")).unwrap(),br#"{"parent_binding":"bound","size":22}"#.to_vec()),
        (".nora-pypi-index/pkg.json".into(),br#"{"schema":2,"name":"pkg","quarantined":false,"project":{"status":{"status":"deprecated","reason":"retained"}}}"#.to_vec())];
    for (key, data) in &extra {
        member.storage.put(key, data).await.unwrap();
    }
    let archive_dir = tempfile::tempdir().unwrap();
    let archive = archive_dir.path().join("backup.tar.gz");
    crate::backup::create_backup(&root, &archive).await.unwrap();
    let restored_dir = tempfile::tempdir().unwrap();
    let restored = Storage::new_local(restored_dir.path().to_str().unwrap());
    crate::backup::restore_backup(&restored, &archive)
        .await
        .unwrap();
    let reopened = RepositoryCatalogue::open(&config(), &restored)
        .await
        .unwrap();
    let copy = reopened.get("remote").unwrap();
    assert_eq!(copy.storage.get(file).await.unwrap().as_ref(), b"retained");
    assert_eq!(copy.storage.pin(file).await, member.storage.pin(file).await);
    assert_eq!(
        copy.storage.get(record).await.unwrap().as_ref(),
        b"source-specific record"
    );
    for (key, data) in &extra {
        assert_eq!(
            copy.storage.get(key).await.unwrap().as_ref(),
            data.as_slice()
        );
        assert_eq!(copy.storage.pin(key).await, member.storage.pin(key).await);
    }
    assert!(restored
        .list("")
        .await
        .unwrap()
        .iter()
        .all(|key| !key.contains(".nora-")));
    let migrated = Storage::from_backend(Arc::new(crate::storage::ObjectStorage::in_memory()));
    let stats = crate::migrate::migrate(&root, &migrated, Default::default())
        .await
        .unwrap();
    assert_eq!(stats.failed, 0);
    let catalogue = RepositoryCatalogue::open(&config(), &migrated)
        .await
        .unwrap();
    let copy = catalogue.get("remote").unwrap();
    assert_eq!(copy.storage.get(file).await.unwrap().as_ref(), b"retained");
    assert_eq!(
        copy.storage.get(record).await.unwrap().as_ref(),
        b"source-specific record"
    );
    for (key, data) in &extra {
        assert_eq!(
            copy.storage.get(key).await.unwrap().as_ref(),
            data.as_slice()
        );
        assert_eq!(copy.storage.pin(key).await, member.storage.pin(key).await);
    }
}

#[tokio::test]
async fn reopening_known_repositories_does_not_enumerate_artifact_trees() {
    let (root, operations) = crate::test_helpers::op_counting_storage();
    RepositoryCatalogue::open(&config(), &root).await.unwrap();
    operations.reset();
    RepositoryCatalogue::open(&config(), &root).await.unwrap();
    assert!(!operations.snapshot().contains_key("list"));
    assert!(!operations.snapshot().contains_key("list_with_meta"));
}

#[tokio::test]
async fn manual_maintenance_previews_include_members_without_creating_namespaces() {
    let root = Storage::from_backend(Arc::new(crate::storage::ObjectStorage::in_memory()));
    let mut cfg = config();
    let preview = RepositoryCatalogue::inspect(&cfg, &root).await.unwrap();
    assert_eq!(preview.members().count(), 2);
    assert!(root
        .list_internal("repositories/")
        .await
        .unwrap()
        .is_empty());
    let catalogue = RepositoryCatalogue::open(&cfg, &root).await.unwrap();
    let local = catalogue.get("local").unwrap();
    local
        .storage
        .put("pypi/pkg/pkg-1.tar.gz.sha256", b"orphan")
        .await
        .unwrap();
    let legacy = [
        (
            "npm/old/metadata.json",
            br#"{"versions":{"1.0.0":{}}}"#.as_slice(),
        ),
        (
            "npm/old/old-1.0.0.tgz.sha256",
            b"legacy checksum".as_slice(),
        ),
        ("pypi/pkg/pkg-0.tar.gz", b"legacy distribution".as_slice()),
        ("pypi/pkg/missing.whl.sha256", b"legacy orphan".as_slice()),
        (
            ".nora-pypi-ready/pkg/missing.whl.json",
            b"legacy proof".as_slice(),
        ),
    ];
    for (key, bytes) in legacy {
        root.put(key, bytes).await.unwrap();
    }
    cfg.gc.grace_secs = 0;
    let locks = Arc::default();
    let mut result = crate::gc::run_root_gc(&root, &locks, true, 0, 0).await;
    catalogue.extend_gc(&cfg, true, &mut result).await;
    assert_eq!(result.orphaned, 1);
    assert!(result.orphan_keys[0].starts_with("local:"));
    assert!(local
        .storage
        .stat("pypi/pkg/pkg-1.tar.gz.sha256")
        .await
        .is_some());
    let mut result = crate::gc::run_root_gc(&root, &locks, false, 0, 0).await;
    catalogue.extend_gc(&cfg, false, &mut result).await;
    assert_eq!(result.deleted, 1);
    local
        .storage
        .put("pypi/pkg/pkg-1.tar.gz", b"distribution")
        .await
        .unwrap();
    cfg.retention.rules = vec![crate::config::RetentionRule {
        registry: "pypi".into(),
        name_glob: None,
        keep_last: Some(0),
        older_than_days: None,
        include_tags: Vec::new(),
        exclude_tags: Vec::new(),
    }];
    let mut result =
        crate::retention::run_root_retention(&root, &locks, None, &cfg.retention.rules, true).await;
    catalogue.extend_retention(&cfg, true, &mut result).await;
    assert_eq!(result.planned, 1);
    assert!(result.plans[0].0.starts_with("local:"));
    assert!(local.storage.stat("pypi/pkg/pkg-1.tar.gz").await.is_some());
    let mut result =
        crate::retention::run_root_retention(&root, &locks, None, &cfg.retention.rules, false)
            .await;
    catalogue.extend_retention(&cfg, false, &mut result).await;
    assert_eq!(result.deleted_keys, 1);
    for (key, bytes) in legacy {
        assert_eq!(root.get(key).await.unwrap().as_ref(), bytes, "{key}");
    }
}

#[tokio::test]
async fn scheduled_member_maintenance_still_retires_only_its_scoped_artifacts_and_companions() {
    let _lock = crate::test_env::env_lock();
    let configured = config();
    let context = crate::test_helpers::with_named_repositories(
        crate::test_helpers::create_test_context_with_config(move |c| {
            c.repositories = configured.repositories;
            c.gc.enabled = true;
            c.gc.grace_secs = 0;
            c.retention.enabled = true;
            c.retention.rules = vec![crate::config::RetentionRule {
                registry: "pypi".into(),
                name_glob: None,
                keep_last: Some(0),
                older_than_days: None,
                include_tags: vec![],
                exclude_tags: vec![],
            }];
        }),
    )
    .await;
    let member = context.state.repositories.get("local").unwrap();
    let orphan = "pypi/demo/missing.whl.sha256";
    member.storage.put(orphan, b"orphan").await.unwrap();
    context.state.repositories.gc(&context.state).await;
    assert!(member.storage.stat(orphan).await.is_none());
    let bodies = [
        "pypi/demo/demo-1.0-py3-none-any.whl",
        "pypi/demo/demo-1.0-py3-none-any.whl.metadata",
    ];
    for body in bodies {
        member.storage.put(body, b"body").await.unwrap();
        member
            .storage
            .put(
                &crate::registry::pypi_ready_key_for_body(body).unwrap(),
                b"proof",
            )
            .await
            .unwrap();
    }
    assert_eq!(member.index.get("pypi", &member.storage).await.len(), 1);
    context.state.repositories.retain(&context.state).await;
    for body in bodies {
        assert!(member.storage.stat(body).await.is_none());
        assert!(member
            .storage
            .stat(&crate::registry::pypi_ready_key_for_body(body).unwrap())
            .await
            .is_none());
    }
    assert!(member.index.get("pypi", &member.storage).await.is_empty());
}

#[tokio::test]
async fn same_upstream_repositories_remain_independent_and_retained_sources_are_reported() {
    let root = Storage::from_backend(Arc::new(crate::storage::ObjectStorage::in_memory()));
    let mut cfg = config();
    let mut second = cfg.repositories["remote"].clone();
    let RepositoryConfig::Proxy(proxy) = &mut second else {
        panic!()
    };
    proxy.paths = vec!["/another/".into()];
    cfg.repositories.insert("another".into(), second);
    let first = RepositoryCatalogue::open(&cfg, &root).await.unwrap();
    assert!(first.retained_namespaces(&root).await.unwrap().is_empty());
    let key = "pypi/pkg/pkg-1.tar.gz";
    first
        .get("remote")
        .unwrap()
        .storage
        .put(key, b"one")
        .await
        .unwrap();
    first
        .get("another")
        .unwrap()
        .storage
        .put(key, b"two")
        .await
        .unwrap();
    assert_eq!(
        first
            .get("remote")
            .unwrap()
            .storage
            .get(key)
            .await
            .unwrap()
            .as_ref(),
        b"one"
    );
    let RepositoryConfig::Proxy(proxy) = cfg.repositories.get_mut("remote").unwrap() else {
        panic!()
    };
    proxy.upstream = "https://new.example/simple/".into();
    cfg.repositories.remove("another");
    let changed = RepositoryCatalogue::open(&cfg, &root).await.unwrap();
    let retained = changed.retained_namespaces(&root).await.unwrap();
    assert!(retained
        .iter()
        .any(|path| path.starts_with("repositories/remote/sources/")));
    assert!(retained.contains(&"repositories/another/".into()));
    assert!(changed
        .get("remote")
        .unwrap()
        .storage
        .list("")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        first
            .get("another")
            .unwrap()
            .storage
            .get(key)
            .await
            .unwrap()
            .as_ref(),
        b"two"
    );
}
