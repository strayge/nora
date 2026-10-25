// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::test_helpers::{self, body_bytes, send, send_with_headers};
use axum::{body::Body, http::Method};
use serde_json::{json, Value};

fn configure(config: &mut crate::config::Config, format: &str) {
    config.repositories=toml::from_str::<crate::config::Config>(&format!("[repositories.a]\nformat='{format}'\nkind='local'\npaths=['/a/','/alias-a/']\n[repositories.b]\nformat='{format}'\nkind='local'\npaths=['/b/']\n[repositories.all]\nformat='{format}'\nkind='group'\npaths=['/all/','/alias-all/']\nmembers=['a','b']\n")).unwrap().repositories;
    config.server.public_url = Some("https://packages.example/base".into());
}

#[tokio::test]
async fn local_release_actions_group_files_rewrite_base_paths_and_preserve_empty_retry_targets() {
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "pypi")
        }))
        .await;
    let storage = &context.state.repositories.get("a").unwrap().storage;
    for file in ["demo-1.0.zip", "demo-1.0-py3-none-any.whl", "demo-2.0.zip"] {
        storage
            .put(&format!("pypi/demo/{file}"), b"archive")
            .await
            .unwrap();
    }
    let package = json_response(&context.app, "/api/ui/repositories/a/package/demo").await;
    assert_eq!(package["releases"].as_array().unwrap().len(), 2);
    assert_eq!(package["releases"][0]["files"], 2);
    assert_eq!(package["stored_versions"].as_array().unwrap().len(), 3);
    let response = send(
        &context.app,
        Method::GET,
        "/ui/repositories/a/package/demo?retry_version=0.5",
        Body::empty(),
    )
    .await;
    let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(html.contains("action=\"/base/api/v1/repositories/a/packages/demo?version=1.0\""));
    assert!(html.contains("Retry version deletion"));
    assert!(html.contains("version=0.5"));
    assert!(html.contains("window.confirm"));
    assert!(html.contains("history.replaceState"));
    assert!(html.contains("Delete package"));
    let html = String::from_utf8(
        body_bytes(
            send(
                &context.app,
                Method::GET,
                "/ui/repositories/a/package/missing?retry_version=1.0",
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(html.contains("No releases are stored"));
    assert!(html.contains("Retry version deletion"));
}

#[tokio::test]
async fn version_api_is_additive_paged_and_rejects_unknown_or_unsafe_versions() {
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "pypi")
        }))
        .await;
    let storage = &context.state.repositories.get("a").unwrap().storage;
    for file in [
        "demo-1.0.tar.gz",
        "demo-1.0-py3-none-any.whl",
        "demo-2.0.tar.gz",
    ] {
        storage
            .put(&format!("pypi/demo/{file}"), b"body")
            .await
            .unwrap();
    }
    let detail = json_response(
        &context.app,
        "/api/ui/repositories/a/package/demo?version=1.0&page=2&limit=1",
    )
    .await;
    assert_eq!(detail["stored_versions"].as_array().unwrap().len(), 3);
    assert_eq!(detail["releases"].as_array().unwrap().len(), 2);
    assert_eq!(detail["artifacts"]["files"]["total"], 2);
    assert_eq!(
        detail["artifacts"]["files"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(detail["artifacts"]["files"]["items"][0]["version"], "1.0");
    for (query, status) in [
        ("version=missing", StatusCode::NOT_FOUND),
        ("version=", StatusCode::BAD_REQUEST),
        ("version=..", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            send(
                &context.app,
                Method::GET,
                &format!("/api/ui/repositories/a/package/demo?{query}"),
                Body::empty()
            )
            .await
            .status(),
            status
        );
    }
}

#[tokio::test]
async fn version_html_has_scoped_navigation_copy_details_and_no_mutation_controls() {
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "pypi")
        }))
        .await;
    let storage = &context.state.repositories.get("a").unwrap().storage;
    for file in [
        "demo-1.0+local.tar.gz",
        "demo-1.0+local-py3-none-any.whl",
        "demo-2.0.tar.gz",
        "odd<script>.whl",
    ] {
        storage
            .put(&format!("pypi/demo/{file}"), b"body")
            .await
            .unwrap();
    }
    storage
        .put(
            "pypi/demo/demo-1.0+local-py3-none-any.whl.sha256",
            "00".repeat(32).as_bytes(),
        )
        .await
        .unwrap();
    let overview = String::from_utf8(
        body_bytes(
            send(
                &context.app,
                Method::GET,
                "/ui/repositories/a/package/demo?limit=1&retry_version=lost",
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(overview.contains("aria-label=\"Next versions\""));
    assert!(overview.contains("retry_version=lost"));
    assert!(overview.contains("odd&lt;script&gt;.whl") && !overview.contains("odd<script>.whl"));
    let html = String::from_utf8(
        body_bytes(
            send(
                &context.app,
                Method::GET,
                "/ui/repositories/a/package/demo?version=1.0%2Blocal&limit=1",
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(html.contains("demo-1.0+local-py3-none-any.whl"));
    assert!(!html.contains("demo-2.0.tar.gz"));
    assert!(html.contains("Copy sha256 checksum") && html.contains("Storage pin"));
    assert!(html.contains("<details open>") && html.contains("Different recorded checksums"));
    assert!(html.contains("/base/ui/repositories/a/package/demo?version=1.0%2Blocal"));
    assert!(html.contains("id=\"package-back\"") && html.contains("aria-label=\"Next files\""));
    assert!(!html.contains("Delete package") && !html.contains("delete-local"));
}

#[tokio::test]
async fn local_mutation_controls_require_a_verified_writer_when_browsing_is_public() {
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_anonymous_read_and_config(
            &[("user", "pass")],
            |c| configure(c, "npm"),
        ),
    )
    .await;
    for (credentials, allowed) in [(None, false), (Some("Basic dXNlcjpwYXNz"), true)] {
        let headers = credentials
            .map(|value| vec![("Authorization", value)])
            .unwrap_or_default();
        let response = send_with_headers(
            &context.app,
            Method::GET,
            "/ui/repositories/a/package/@scope%2Fdemo",
            headers,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        assert_eq!(html.contains("Delete package"), allowed);
    }
    let token = context
        .state
        .tokens
        .as_ref()
        .unwrap()
        .create_token("user", 1, None, crate::tokens::Role::Read)
        .unwrap();
    let header = format!("Bearer {token}");
    let html = String::from_utf8(
        body_bytes(
            send_with_headers(
                &context.app,
                Method::GET,
                "/ui/repositories/a/package/demo",
                vec![("Authorization", &header)],
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("Delete package"));
    let html = String::from_utf8(
        body_bytes(
            send_with_headers(
                &context.app,
                Method::GET,
                "/ui/repositories/all/package/demo",
                vec![("Authorization", "Basic dXNlcjpwYXNz")],
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("delete-local"));
}

async fn json_response(app: &Router, path: &str) -> Value {
    let response = send(app, Method::GET, path, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

#[tokio::test]
async fn pypi_browser_lists_every_supported_distribution_without_merging_group_members_or_legacy_bytes(
) {
    let _lock = crate::test_env::env_lock();
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "pypi")
        }))
        .await;
    let a = context.state.repositories.get("a").unwrap();
    for file in [
        "demo-1.0.tar.gz",
        "demo-1.1.tgz",
        "demo-1.2.zip",
        "demo-1.3-py3-none-any.whl",
        "demo-1.4-py3.egg",
    ] {
        a.storage
            .put(&format!("pypi/demo/{file}"), b"a")
            .await
            .unwrap();
    }
    a.storage
        .put("pypi/demo/demo-1.3-py3-none-any.whl.metadata", b"sidecar")
        .await
        .unwrap();
    context
        .state
        .repositories
        .get("b")
        .unwrap()
        .storage
        .put("pypi/demo/demo-2.0.tar.gz", b"b")
        .await
        .unwrap();
    context
        .state
        .storage
        .put("pypi/legacy/legacy-0.1.tar.gz", b"legacy")
        .await
        .unwrap();
    let index = json_response(&context.app, "/api/ui/repositories").await;
    assert_eq!(index.as_array().unwrap().len(), 3);
    let group = index
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "all")
        .unwrap();
    assert!(group["stored_artifacts"].is_null());
    let detail = json_response(&context.app, "/api/ui/repositories/all/package/Demo").await;
    assert_eq!(detail["source_repository"], "a");
    assert_eq!(detail["stored_versions"].as_array().unwrap().len(), 5);
    assert_eq!(detail["commands"].as_array().unwrap().len(), 2);
    for command in detail["commands"].as_array().unwrap() {
        assert!(command.get("publish").is_none());
        assert!(command["install"]
            .as_str()
            .unwrap()
            .contains("https://packages.example/base/"));
    }
    let stats = json_response(&context.app, "/api/ui/stats").await;
    assert_eq!(stats["pypi"], 2);
    context
        .state
        .repositories
        .refresh_gauges(&context.state.enabled_registries)
        .await;
    assert_eq!(
        crate::metrics::REPOSITORY_ARTIFACTS
            .with_label_values(&["a", "pypi"])
            .get(),
        5
    );
    assert_eq!(
        crate::metrics::ARTIFACTS_TOTAL
            .with_label_values(&["pypi"])
            .get(),
        2
    );
    for family in prometheus::gather()
        .into_iter()
        .filter(|family| family.name() == "nora_repository_artifacts")
    {
        assert!(!family.get_metric().iter().any(|metric| metric
            .get_label()
            .iter()
            .any(|label| label.name() == "repository" && label.value() == "all")));
    }
    let dashboard = json_response(&context.app, "/api/ui/dashboard").await;
    let pypi = dashboard["registry_stats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "pypi")
        .unwrap();
    assert_eq!(pypi["artifact_count"], 6);
    assert!(!dashboard["mount_points"]
        .as_array()
        .unwrap()
        .iter()
        .any(|mount| mount["mount_path"] == "/simple/"));
    let html = String::from_utf8(
        body_bytes(
            send(
                &context.app,
                Method::GET,
                "/ui/repositories/all/package/demo",
                Body::empty(),
            )
            .await,
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("selected source:")
            && html.contains("/base/ui/repositories/a/package/demo")
            && html.contains("offline readiness")
    );
    for path in [
        "/api/ui/pypi/list",
        "/api/ui/pypi/legacy",
        "/api/ui/pypi/search?q=legacy",
    ] {
        assert_eq!(
            send(&context.app, Method::GET, path, Body::empty())
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn npm_browser_keeps_scoped_packages_and_commands_bound_to_the_requested_repository() {
    use base64::Engine as _;
    let _lock = crate::test_env::env_lock();
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "npm");
            c.audit.mode = crate::audit::AuditMode::File;
        }))
        .await;
    for (repo, version) in [("a", "1.0.0"), ("b", "2.0.0")] {
        let file = format!("pkg-{version}.tgz");
        let document = json!({"name":"@scope/pkg","dist-tags":{"latest":version},"versions":{version:{"name":"@scope/pkg","version":version,"dist":{"tarball":format!("http://unused/@scope/pkg/-/{file}")}}},"_attachments":{file:{"data":base64::engine::general_purpose::STANDARD.encode(b"tgz"),"length":3}}});
        let response = send_with_headers(
            &context.app,
            Method::PUT,
            &format!("/{repo}/@scope%2Fpkg"),
            vec![("content-type", "application/json")],
            Body::from(document.to_string()),
        )
        .await;
        assert!(
            response.status().is_success(),
            "{}",
            String::from_utf8_lossy(&body_bytes(response).await)
        );
    }
    let detail = json_response(
        &context.app,
        "/api/ui/repositories/all/package/%40scope%2Fpkg",
    )
    .await;
    assert_eq!(detail["source_repository"], "a");
    assert_eq!(detail["stored_versions"].as_array().unwrap().len(), 1);
    assert_eq!(detail["stored_versions"][0]["version"], "1.0.0");
    let local = json_response(
        &context.app,
        "/api/ui/repositories/a/package/%40scope%2Fpkg",
    )
    .await;
    assert!(local["commands"][1]["publish"]
        .as_str()
        .unwrap()
        .contains("https://packages.example/base/alias-a/"));
    assert_eq!(
        send(&context.app, Method::GET, "/api/ui/npm/list", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    context.state.audit.shutdown().await;
    let audit = std::fs::read_to_string(context.state.audit.path()).unwrap();
    let pushes: Vec<Value> = audit
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["action"] == "push")
        .collect();
    assert_eq!(pushes.len(), 2);
    for (entry, repository) in pushes.iter().zip(["a", "b"]) {
        assert_eq!(entry["repository"], repository);
        assert_eq!(entry["source_id"], "local");
        assert_eq!(entry["registry"], "npm");
    }
}

#[tokio::test]
async fn client_commands_terminate_options_before_a_leading_hyphen_coordinate() {
    let _lock = crate::test_env::env_lock();
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "npm")
        }))
        .await;
    let package = package_data(&context.state, "a", "--global", &PackageQuery::default())
        .await
        .unwrap();
    let command = &package.commands[0].install;
    let output = std::process::Command::new("sh")
        .args(["-c", &format!("set -- {command}; printf '%s\\n' \"$@\"")])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let args: Vec<_> = text.lines().collect();
    assert_eq!(
        args,
        [
            "npm",
            "install",
            "--registry",
            "https://packages.example/base/a/",
            "--",
            "--global"
        ]
    );
}

#[tokio::test]
async fn repository_pagination_and_filter_keep_the_effective_limit() {
    let _lock = crate::test_env::env_lock();
    let context =
        test_helpers::with_named_repositories(test_helpers::create_test_context_with_config(|c| {
            configure(c, "pypi")
        }))
        .await;
    let member = context.state.repositories.get("a").unwrap();
    for name in ["one", "two", "three"] {
        member
            .storage
            .put(&format!("pypi/{name}/{name}-1.0.whl"), b"body")
            .await
            .unwrap();
    }
    let html = String::from_utf8(
        body_bytes(send(&context.app, Method::GET, "/ui/repositories/a?limit=1", "").await)
            .await
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("?page=2&amp;limit=1&amp;q=") && html.contains("name=\"limit\" value=\"1\"")
    );
    let page = json_response(&context.app, "/api/ui/repositories/a?page=2&limit=1").await;
    assert_eq!(page["packages"], json!(["three"]));
}

#[tokio::test]
async fn repository_browser_keeps_existing_auth_and_has_no_mutating_routes() {
    let _lock = crate::test_env::env_lock();
    let context = test_helpers::with_named_repositories(
        test_helpers::create_test_context_with_auth_and_config(&[("user", "pass")], |c| {
            configure(c, "pypi")
        }),
    )
    .await;
    assert_eq!(
        send(
            &context.app,
            Method::GET,
            "/api/ui/repositories",
            Body::empty()
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    for (method, path, expected) in [
        (Method::GET, "/api/ui/repositories", StatusCode::OK),
        (
            Method::POST,
            "/api/ui/repositories/a",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::DELETE,
            "/ui/repositories/a",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ] {
        let response = send_with_headers(
            &context.app,
            method,
            path,
            vec![("authorization", "Basic dXNlcjpwYXNz")],
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), expected, "{path}");
    }
    assert_eq!(shell_arg("a'b;$(x)"), "'a'\\''b;$(x)'");
    assert!(!command_block("<script>'&").contains("<script>"));
}
