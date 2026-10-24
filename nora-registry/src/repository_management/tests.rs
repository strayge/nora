// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::test_helpers::{self, body_bytes, send, send_with_headers, TestContext};
use axum::{body::Body, http::Method};
use serde_json::{json, Value};

fn configure(config: &mut crate::config::Config) {
    config.repositories = toml::from_str::<crate::config::Config>(
        "
[repositories.n]
format='npm'
kind='local'
paths=['/n/']
[repositories.n2]
format='npm'
kind='local'
paths=['/n2/']
[repositories.p]
format='pypi'
kind='local'
paths=['/p/']
[repositories.proxy]
format='npm'
kind='proxy'
paths=['/proxy/']
upstream='http://127.0.0.1:1/'
[repositories.group]
format='npm'
kind='group'
paths=['/group/']
members=['n','proxy']
",
    )
    .unwrap()
    .repositories;
}

async fn context() -> TestContext {
    test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(configure))
        .await
}

async fn remove(context: &TestContext, path: &str) -> (StatusCode, Value) {
    let response = send_with_headers(
        &context.app,
        Method::DELETE,
        path,
        vec![("HX-Request", "true")],
        Body::empty(),
    )
    .await;
    let status = response.status();
    let bytes = body_bytes(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn seed_npm(context: &TestContext, repository: &str, name: &str, version: &str) {
    let storage = &context.state.repositories.get(repository).unwrap().storage;
    let short = name.rsplit('/').next().unwrap();
    storage
        .put(
            &format!("npm/{name}/tarballs/{short}-{version}.tgz"),
            b"tarball",
        )
        .await
        .unwrap();
    storage
        .put(
            &format!("npm/{name}/versions/{version}.json"),
            json!({"name":name,"version":version,"dist":{}})
                .to_string()
                .as_bytes(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn npm_version_cleanup_is_isolated_reconciles_tags_and_removes_final_package() {
    let context = context().await;
    for (repo, version) in [("n", "1.0.0"), ("n", "2.0.0"), ("n2", "2.0.0")] {
        seed_npm(&context, repo, "@s/demo", version).await;
    }
    let storage = &context.state.repositories.get("n").unwrap().storage;
    storage
        .put("npm/@s/demo/dist-tags/latest", b"2.0.0")
        .await
        .unwrap();
    storage
        .put("npm/@s/demo/dist-tags/beta", b"2.0.0")
        .await
        .unwrap();
    storage
        .put("npm/@s/demo/tarballs/notes.zip", b"extra")
        .await
        .unwrap();
    let (status, result) = remove(
        &context,
        "/api/v1/repositories/n/packages/@s%2Fdemo?version=2.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["deleted_files"], 1);
    assert_eq!(result["residual_files"], 1);
    let document: Value =
        serde_json::from_slice(&storage.get("npm/@s/demo/metadata.json").await.unwrap()).unwrap();
    assert_eq!(document["dist-tags"]["latest"], "1.0.0");
    assert!(document["dist-tags"].get("beta").is_none());
    assert!(storage
        .get("npm/@s/demo/tarballs/demo-2.0.0.tgz")
        .await
        .is_err());
    assert!(context
        .state
        .repositories
        .get("n2")
        .unwrap()
        .storage
        .get("npm/@s/demo/versions/2.0.0.json")
        .await
        .is_ok());
    let (status, result) = remove(
        &context,
        "/api/v1/repositories/n/packages/@s/demo?version=1.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["package_removed"], true);
    assert!(storage.list("npm/@s/demo/").await.unwrap().is_empty());
    assert!(storage
        .list_internal(".nora-origins/npm/@s/demo/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        remove(
            &context,
            "/api/v1/repositories/n/packages/@s/demo?version=1.0.0"
        )
        .await
        .1["status"],
        "already_absent"
    );
}

#[tokio::test]
async fn pypi_release_removes_all_files_and_orphan_companions_but_not_other_release() {
    let context = context().await;
    let storage = &context.state.repositories.get("p").unwrap().storage;
    for file in [
        "demo-1.0.tar.gz",
        "demo-1.0-py3-none-any.whl",
        "demo-1.0.0.zip",
    ] {
        storage
            .put(&format!("pypi/demo/{file}"), b"archive")
            .await
            .unwrap();
        for key in [
            format!("pypi/demo/{file}.sha256"),
            format!("pypi/demo/{file}.metadata"),
            format!("pypi-sha256/demo/{file}/abc"),
            format!(".nora-pypi-ready/demo/{file}.json"),
        ] {
            storage.put(&key, b"companion").await.unwrap();
        }
    }
    storage
        .put("pypi/demo/demo-1.0.tgz.sha256", b"orphan")
        .await
        .unwrap();
    let (status, result) =
        remove(&context, "/api/v1/repositories/p/packages/Demo?version=1.0").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["deleted_files"], 2);
    assert!(!storage
        .list("pypi/demo/")
        .await
        .unwrap()
        .iter()
        .any(|key| key.contains("1.0-") || key.contains("1.0.tar") || key.contains("1.0.tgz")));
    assert!(storage.get("pypi/demo/demo-1.0.0.zip").await.is_ok());
    assert!(storage
        .list_internal(".nora-pypi-ready/demo/demo-1.0.tar.gz")
        .await
        .unwrap()
        .is_empty());
    assert!(storage
        .list("pypi-sha256/demo/demo-1.0.tar.gz/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        remove(&context, "/api/v1/repositories/p/packages/demo")
            .await
            .1["package_removed"],
        true
    );
}

#[tokio::test]
async fn deletion_refuses_public_browse_nonlocals_unsafe_targets_and_busy_maintenance() {
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_anonymous_read_and_config(
            &[("user", "pass")],
            |config| {
                configure(config);
                config.auth.public_web_ui = true;
            },
        ),
    )
    .await;
    seed_npm(&context, "n", "demo", "1.0.0").await;
    let url = "/api/v1/repositories/n/packages/demo";
    assert_eq!(remove(&context, url).await.0, StatusCode::UNAUTHORIZED);
    let basic = "Basic dXNlcjpwYXNz";
    let response = send_with_headers(
        &context.app,
        Method::DELETE,
        url,
        vec![("Authorization", basic)],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    for (path, expected) in [
        ("/api/v1/repositories/group/packages/demo", 405),
        ("/api/v1/repositories/proxy/packages/demo", 405),
        ("/api/v1/repositories/n/packages/demo?version=..", 400),
    ] {
        let response = send_with_headers(
            &context.app,
            Method::DELETE,
            path,
            vec![("Authorization", basic), ("HX-Request", "true")],
            Body::empty(),
        )
        .await;
        assert_eq!(response.status().as_u16(), expected);
    }
    let gate = context.state.cleanup_lock.lock().await;
    let response = send_with_headers(
        &context.app,
        Method::DELETE,
        url,
        vec![("Authorization", basic), ("HX-Request", "true")],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["Retry-After"], "2");
    drop(gate);
    assert_eq!(
        send(&context.app, Method::GET, url, Body::empty())
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let response = send_with_headers(
        &context.app,
        Method::DELETE,
        url,
        vec![("Authorization", basic), ("HX-Request", "true")],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn storage_failures_are_honest_and_orphan_only_retries_complete() {
    let (storage, counts) = test_helpers::op_counting_storage();
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_storage_and_config(storage, configure),
    )
    .await;
    seed_npm(&context, "n", "demo", "1.0").await;
    seed_npm(&context, "n", "demo", "2.0").await;
    let storage = &context.state.repositories.get("n").unwrap().storage;
    let url = "/api/v1/repositories/n/packages/demo?version=1.0";
    let physical = "repositories/n/sources/local/";
    counts.fail_reads_to(Some(&format!("{physical}npm/demo/")));
    assert_eq!(
        remove(&context, url).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    counts.fail_reads_to(None);
    counts.fail_deletes_to(Some(&format!("{physical}npm/demo/tarballs/demo-1.0.tgz")));
    assert_eq!(
        remove(&context, url).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(storage.get("npm/demo/versions/1.0.json").await.is_ok());
    assert!(storage.get("npm/demo/tarballs/demo-1.0.tgz").await.is_ok());
    counts.fail_deletes_to(Some(&format!("{physical}npm/demo/versions/1.0.json")));
    assert_eq!(
        remove(&context, url).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(storage.get("npm/demo/tarballs/demo-1.0.tgz").await.is_err());
    counts.fail_deletes_to(None);
    counts.fail_writes_to(Some(&format!(
        "{physical}.nora-origins/npm/demo/metadata.json"
    )));
    assert_eq!(
        remove(&context, url).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    counts.fail_writes_to(None);
    assert_eq!(remove(&context, url).await.0, StatusCode::OK);
    let document: Value =
        serde_json::from_slice(&storage.get("npm/demo/metadata.json").await.unwrap()).unwrap();
    assert!(document["versions"].get("2.0").is_some());
    assert!(document["versions"].get("1.0").is_none());

    let storage = &context.state.repositories.get("p").unwrap().storage;
    storage
        .put("pypi/demo/demo-1.0.zip", b"bytes")
        .await
        .unwrap();
    storage
        .put("pypi/demo/demo-1.0.zip.sha256", b"digest")
        .await
        .unwrap();
    storage
        .put("pypi/demo/demo-2.0.zip", b"keep")
        .await
        .unwrap();
    counts.fail_deletes_to(Some(
        "repositories/p/sources/local/pypi/demo/demo-1.0.zip.sha256",
    ));
    let url = "/api/v1/repositories/p/packages/demo?version=1.0";
    assert_eq!(
        remove(&context, url).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(storage.get("pypi/demo/demo-1.0.zip").await.is_err());
    counts.fail_deletes_to(None);
    assert_eq!(remove(&context, url).await.0, StatusCode::OK);
    assert!(storage.get("pypi/demo/demo-1.0.zip.sha256").await.is_err());
    assert!(storage.get("pypi/demo/demo-2.0.zip").await.is_ok());
}

#[tokio::test]
async fn dropped_request_keeps_the_critical_section_until_storage_settles() {
    let (storage, counts) = test_helpers::op_counting_storage();
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_storage_and_config(storage, configure),
    )
    .await;
    seed_npm(&context, "n", "demo", "1.0").await;
    let (started, proceed) =
        counts.pause_deletes_to("repositories/n/sources/local/npm/demo/tarballs/demo-1.0.tgz");
    let app = context.app.clone();
    let request = tokio::spawn(async move {
        send_with_headers(
            &app,
            Method::DELETE,
            "/api/v1/repositories/n/packages/demo",
            vec![("HX-Request", "true")],
            Body::empty(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    request.abort();
    let _ = request.await;
    assert!(context.state.cleanup_lock.try_lock().is_err());
    let member = context
        .state
        .repositories
        .get("n")
        .unwrap()
        .state(&context.state);
    let package_lock = member.publish_lock("npm/demo/metadata.json");
    assert!(package_lock.try_lock().is_err());
    assert_eq!(
        remove(&context, "/api/v1/repositories/n/packages/demo")
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    proceed.notify_one();
    context.state.background_tasks.close();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        context.state.background_tasks.wait(),
    )
    .await
    .unwrap();
    assert!(context.state.cleanup_lock.try_lock().is_ok());
    assert!(package_lock.try_lock().is_ok());
    assert!(member.storage.list("npm/demo/").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_new_pypi_filename_waits_for_package_cleanup_then_keeps_its_hash() {
    let (storage, counts) = test_helpers::op_counting_storage();
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_storage_and_config(storage, configure),
    )
    .await;
    let member = context
        .state
        .repositories
        .get("p")
        .unwrap()
        .state(&context.state);
    member
        .storage
        .put("pypi/demo/demo-1.0.zip", b"old")
        .await
        .unwrap();
    let (started, proceed) =
        counts.pause_deletes_to("repositories/p/sources/local/pypi/demo/demo-1.0.zip");
    let app = context.app.clone();
    let deletion = tokio::spawn(async move {
        send_with_headers(
            &app,
            Method::DELETE,
            "/api/v1/repositories/p/packages/demo",
            vec![("HX-Request", "true")],
            Body::empty(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let body = "--boundary\r\nContent-Disposition: form-data; name=\":action\"\r\n\r\nfile_upload\r\n--boundary\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\ndemo\r\n--boundary\r\nContent-Disposition: form-data; name=\"version\"\r\n\r\n2.0\r\n--boundary\r\nContent-Disposition: form-data; name=\"content\"; filename=\"demo-2.0.zip\"\r\nContent-Type: application/octet-stream\r\n\r\nreplacement\r\n--boundary--\r\n";
    let upload = send_with_headers(
        &context.app,
        Method::POST,
        "/p/",
        vec![("Content-Type", "multipart/form-data; boundary=boundary")],
        Body::from(body),
    );
    tokio::pin!(upload);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), upload.as_mut())
            .await
            .is_err()
    );
    proceed.notify_one();
    assert_eq!(deletion.await.unwrap().status(), StatusCode::OK);
    assert_eq!(upload.await.status(), StatusCode::OK);
    assert_eq!(
        member
            .storage
            .get("pypi/demo/demo-2.0.zip")
            .await
            .unwrap()
            .as_ref(),
        b"replacement"
    );
    let markers = member
        .storage
        .list("pypi-sha256/demo/demo-2.0.zip/")
        .await
        .unwrap();
    assert_eq!(markers.len(), 1);
    use sha2::Digest as _;
    assert!(markers[0].ends_with(&hex::encode(sha2::Sha256::digest(b"replacement"))));
}

#[tokio::test]
async fn write_roles_and_enforced_namespace_scope_are_required() {
    use crate::tokens::Role;
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_auth_and_config(&[("user", "pass")], configure),
    )
    .await;
    let url = "/api/v1/repositories/n/packages/demo";
    for (role, expected) in [
        (Role::Read, StatusCode::FORBIDDEN),
        (Role::Write, StatusCode::OK),
        (Role::Admin, StatusCode::OK),
    ] {
        let token = context
            .state
            .tokens
            .as_ref()
            .unwrap()
            .create_token("user", 1, None, role)
            .unwrap();
        let bearer = format!("Bearer {token}");
        assert_eq!(
            send_with_headers(
                &context.app,
                Method::DELETE,
                url,
                vec![("Authorization", &bearer), ("HX-Request", "true")],
                Body::empty()
            )
            .await
            .status(),
            expected
        );
    }
    // Exercise the handler's canonical-coordinate authorization independently of JWT issuance.
    let authority = NamespaceAuthority::from_oidc_scope(
        "ci",
        &["@allowed/*".into()],
        crate::config::ScopeEnforcement::Enforce,
    );
    let app = routes()
        .layer(Extension(authority))
        .layer(Extension(AuthenticatedUser("scoped".into())))
        .layer(Extension(AuthenticatedRole(Role::Write)))
        .with_state(context.state.clone());
    assert_eq!(
        send_with_headers(
            &app,
            Method::DELETE,
            url,
            vec![("HX-Request", "true")],
            Body::empty()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send_with_headers(
            &app,
            Method::DELETE,
            "/api/v1/repositories/n/packages/@allowed/demo",
            vec![("HX-Request", "true")],
            Body::empty()
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn corrupt_target_can_be_removed_while_stored_only_survivors_are_not_adopted() {
    let context = context().await;
    let storage = &context.state.repositories.get("n").unwrap().storage;
    seed_npm(&context, "n", "demo", "1.0").await;
    storage
        .put("npm/demo/versions/1.0.json", b"broken")
        .await
        .unwrap();
    storage
        .put("npm/demo/tarballs/2.0/demo-2.0.tgz", b"untracked")
        .await
        .unwrap();
    storage
        .put("npm/demo/metadata.json", b"broken")
        .await
        .unwrap();
    let (status, result) =
        remove(&context, "/api/v1/repositories/n/packages/demo?version=1.0").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["package_removed"], false);
    assert!(storage.get("npm/demo/metadata.json").await.is_err());
    assert!(storage
        .get("npm/demo/tarballs/2.0/demo-2.0.tgz")
        .await
        .is_ok());
    // Repeating an already-absent release delete must not sweep a nested survivor.
    assert_eq!(
        remove(&context, "/api/v1/repositories/n/packages/demo?version=1.0")
            .await
            .0,
        StatusCode::OK
    );
    assert!(storage
        .get("npm/demo/tarballs/2.0/demo-2.0.tgz")
        .await
        .is_ok());
    assert!(storage.list("npm/demo/versions/").await.unwrap().is_empty());
    assert_eq!(
        remove(&context, "/api/v1/repositories/n/packages/demo")
            .await
            .0,
        StatusCode::OK
    );
}
