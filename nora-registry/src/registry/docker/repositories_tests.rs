// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, send, with_named_repositories, TestContext,
};
use sha2::Digest;

async fn context(tables: &str) -> TestContext {
    let config: crate::config::Config = toml::from_str(tables).unwrap();
    with_named_repositories(create_test_context_with_config(|c| {
        c.server.port = 4000;
        c.repositories = config.repositories;
    }))
    .await
}

fn router(ctx: &TestContext, name: &str) -> Router {
    let member = ctx.state.repositories.get(name).unwrap();
    let view = member.state(&ctx.state);
    crate::request_layers(member.docker_routes(&ctx.state), &view).with_state(view)
}

fn manifest(label: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[],"annotations":{"label":label}})).unwrap()
}

const LOCALS: &str = r#"
[repositories.a]
format = "docker"
kind = "local"
[repositories.b]
format = "docker"
kind = "local"
port = 19443
public_url = "https://images.example.com:9443"
"#;

#[tokio::test]
async fn main_and_extra_port_routers_preserve_images_and_dotted_names() {
    let _lock = crate::test_env::env_lock();
    let ctx = context(LOCALS).await;
    let b = router(&ctx, "b");
    for (app, label) in [(&ctx.app, "a"), (&b, "b")] {
        for image in ["app", "docker.io/app"] {
            let bytes = manifest(&format!("{label}/{image}"));
            let path = format!("/v2/{image}/manifests/latest");
            let pushed = send(app, Method::PUT, &path, bytes.clone()).await;
            assert_eq!(pushed.status(), StatusCode::CREATED);
            assert_eq!(pushed.headers()[header::CONTENT_SECURITY_POLICY], "sandbox");
            assert_eq!(
                body_bytes(send(app, Method::GET, &path, Vec::new()).await).await,
                bytes
            );
        }
    }
    assert_eq!(
        send(&ctx.app, Method::GET, "/v2/", Vec::new())
            .await
            .status(),
        StatusCode::OK
    );
    for repository in ["a", "b"] {
        let view = ctx
            .state
            .repositories
            .get(repository)
            .unwrap()
            .state(&ctx.state);
        let index = view.repo_index.get("docker", &view.storage).await;
        assert_eq!(
            index.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["app", "docker.io/app"]
        );
        let detail = crate::ui::api::get_docker_detail(&view, "app")
            .await
            .unwrap();
        assert_eq!(detail.tags.len(), 1);
        assert!(detail.tags[0].pull_command.ends_with("/app:latest"));
    }
    let api = send(
        &ctx.app,
        Method::GET,
        "/api/ui/repositories/b/package/docker.io/app",
        Vec::new(),
    )
    .await;
    assert_eq!(api.status(), StatusCode::OK);
    let data: Value = serde_json::from_slice(&body_bytes(api).await).unwrap();
    assert!(data["tags"][0]["pull_command"]
        .as_str()
        .unwrap()
        .contains("images.example.com:9443/docker.io/app:latest"));
    assert!(ctx.state.storage.list("docker/").await.unwrap().is_empty());
}

#[tokio::test]
async fn recursive_tag_listing_never_includes_a_different_image() {
    let _lock = crate::test_env::env_lock();
    let ctx = context(LOCALS).await;
    for (image, tag) in [("x", "parent"), ("x/manifests/y", "child")] {
        let path = format!("/v2/{image}/manifests/{tag}");
        assert_eq!(
            send(&ctx.app, Method::PUT, &path, manifest(image))
                .await
                .status(),
            StatusCode::CREATED
        );
    }
    let view = ctx.state.repositories.get("a").unwrap().state(&ctx.state);
    for (image, tag) in [("x", "parent"), ("x/manifests/y", "child")] {
        let detail = crate::ui::api::get_docker_detail(&view, image)
            .await
            .unwrap();
        assert_eq!(detail.tags.len(), 1);
        assert_eq!(detail.tags[0].name, tag);
        assert!(detail.tags[0]
            .pull_command
            .ends_with(&format!("/{image}:{tag}")));
    }
}

#[tokio::test]
async fn upload_uuid_cannot_be_used_or_cancelled_on_another_repository() {
    let _lock = crate::test_env::env_lock();
    let ctx = context(LOCALS).await;
    let b = router(&ctx, "b");
    let started = send(&ctx.app, Method::POST, "/v2/app/blobs/uploads/", Vec::new()).await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let location = started.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(b"hello")));
    for (method, path) in [
        (Method::PATCH, location.clone()),
        (Method::PUT, format!("{location}?digest={digest}")),
        (Method::DELETE, location.clone()),
    ] {
        assert_eq!(
            send(&b, method, &path, b"hello".to_vec()).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(&ctx.app, Method::PATCH, &location, b"hello".to_vec())
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        send(
            &ctx.app,
            Method::PUT,
            &format!("{location}?digest={digest}"),
            Vec::new()
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &b,
            Method::GET,
            &format!("/v2/app/blobs/{digest}"),
            Vec::new()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn proxy_revalidates_bare_key_tags_and_refuses_all_writes() {
    let _lock = crate::test_env::env_lock();
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    let config = format!("[repositories.cache]\nformat='docker'\nkind='proxy'\nport=19443\nupstream='{}'\nmetadata_ttl=0\n", upstream.uri());
    let ctx = context(&config).await;
    let app = router(&ctx, "cache");
    let view = ctx
        .state
        .repositories
        .get("cache")
        .unwrap()
        .state(&ctx.state);
    for (label, expected) in [("one", 1), ("two", 2)] {
        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/v2/app/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest(label)))
            .mount(&upstream)
            .await;
        let response = send(&app, Method::GET, "/v2/app/manifests/latest", Vec::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, manifest(label));
        for _ in 0..100 {
            if view
                .storage
                .get("docker/app/manifests/latest.json")
                .await
                .is_ok_and(|b| b == manifest(label))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            view.storage
                .get("docker/app/manifests/latest.json")
                .await
                .unwrap(),
            manifest(label)
        );
        assert_eq!(
            view.repo_index.get("docker", &view.storage).await.len(),
            1,
            "round {expected}"
        );
    }
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        assert_eq!(
            send(&app, method, "/v2/app/manifests/latest", Vec::new())
                .await
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    assert_eq!(
        send(&ctx.app, Method::GET, "/v2/", Vec::new())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let digest = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(manifest("two")))
    );
    let key = format!("docker/app/manifests/{digest}.json");
    for _ in 0..100 {
        if view.storage.get(&key).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(view.storage.get(&key).await.unwrap(), manifest("two"));
}

#[tokio::test]
async fn positive_proxy_ttl_holds_only_until_expiry() {
    let _lock = crate::test_env::env_lock();
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let upstream = MockServer::start().await;
    let ctx = context(&format!(
        "[repositories.cache]\nformat='docker'\nkind='proxy'\nupstream='{}'\nmetadata_ttl=2\n",
        upstream.uri()
    ))
    .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest("one")))
        .mount(&upstream)
        .await;
    assert_eq!(
        body_bytes(
            send(
                &ctx.app,
                Method::GET,
                "/v2/app/manifests/latest",
                Vec::new()
            )
            .await
        )
        .await,
        manifest("one")
    );
    let view = ctx
        .state
        .repositories
        .get("cache")
        .unwrap()
        .state(&ctx.state);
    for _ in 0..100 {
        if view
            .storage
            .get("docker/app/manifests/latest.json")
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    upstream.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest("two")))
        .mount(&upstream)
        .await;
    assert_eq!(
        body_bytes(
            send(
                &ctx.app,
                Method::GET,
                "/v2/app/manifests/latest",
                Vec::new()
            )
            .await
        )
        .await,
        manifest("one")
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        body_bytes(
            send(
                &ctx.app,
                Method::GET,
                "/v2/app/manifests/latest",
                Vec::new()
            )
            .await
        )
        .await,
        manifest("two")
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn retained_root_data_survives_destructive_maintenance_and_is_not_inventory() {
    let _lock = crate::test_env::env_lock();
    let ctx = context(LOCALS).await;
    ctx.state
        .storage
        .put("docker/old/blobs/sha256:orphan", b"retained")
        .await
        .unwrap();
    ctx.state
        .storage
        .put("docker/old/manifests/v1.json", &manifest("old"))
        .await
        .unwrap();
    crate::gc::run_root_gc(&ctx.state.storage, &ctx.state.publish_locks, false, 0, 0).await;
    crate::retention::run_root_retention(
        &ctx.state.storage,
        &ctx.state.publish_locks,
        None,
        &[],
        false,
    )
    .await;
    assert_eq!(
        ctx.state
            .storage
            .get("docker/old/blobs/sha256:orphan")
            .await
            .unwrap(),
        b"retained"[..]
    );
    let stats = crate::ui::api::api_stats(State(ctx.state.clone())).await.0;
    assert_eq!(stats.docker, 0);
    let response = send(&ctx.app, Method::GET, "/api/ui/docker", Vec::new()).await;
    assert_ne!(response.status(), StatusCode::OK);
}
