// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use crate::config::Config;
use crate::test_helpers::{
    body_bytes, create_test_context_with_config, create_test_context_with_storage_and_config, send,
    send_with_headers, with_named_repositories, TestContext,
};
use axum::http::{Method, StatusCode};
use base64::Engine;
use serde_json::{json, Value};
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Npm,
    Pypi,
}
impl Format {
    fn name(self) -> &'static str {
        if self == Self::Npm {
            "npm"
        } else {
            "pypi"
        }
    }
    fn metadata(self, prefix: &str, name: &str) -> String {
        format!(
            "{prefix}{name}{}",
            if self == Self::Pypi { "/" } else { "" }
        )
    }
    fn filename(self, name: &str, version: &str) -> String {
        let name = name.rsplit('/').next().unwrap();
        if self == Self::Npm {
            format!("{name}-{version}.tgz")
        } else {
            format!("{}-{version}-py3-none-any.whl", name.replace('-', "_"))
        }
    }
    fn artifact(self, prefix: &str, name: &str, version: &str) -> String {
        format!(
            "{prefix}{name}/{}{}",
            if self == Self::Npm { "-/" } else { "" },
            self.filename(name, version)
        )
    }
    fn upstream_path(self, name: &str) -> String {
        if self == Self::Npm {
            format!("/{name}")
        } else {
            format!("/simple/{name}/")
        }
    }
}

fn configuration(
    format: Format,
    sources: Option<(&str, &str)>,
    reversed: bool,
    owner: &str,
) -> Config {
    let proxy = |source: &str| {
        format!(
            "upstream=\"{}{}\"\nmetadata_ttl=0\nmetadata_timeout=1\nread_timeout=1\n",
            source,
            if format == Format::Pypi {
                "/simple"
            } else {
                ""
            }
        )
    };
    let (kind, a, b) = match sources {
        Some((a, b)) => ("proxy", proxy(a), proxy(b)),
        None => ("local", String::new(), String::new()),
    };
    let owners = if owner.is_empty() {
        String::new()
    } else {
        format!("owners={owner}\n")
    };
    toml::from_str(&format!(
        r#"
[repositories.a]
format="{format}"
kind="{kind}"
paths=["/a/"]
{a}
[repositories.b]
format="{format}"
kind="{kind}"
paths=["/b/"]
{b}
[repositories.all]
format="{format}"
kind="group"
paths=["/group/","/alias/"]
members={members}
{owners}
[repositories.other]
format="{format}"
kind="group"
paths=["/other/"]
members=["a","b"]
"#,
        format = format.name(),
        members = if reversed {
            "[\"b\",\"a\"]"
        } else {
            "[\"a\",\"b\"]"
        }
    ))
    .unwrap()
}
async fn context(config: Config, storage: Option<crate::Storage>) -> TestContext {
    let customize = |target: &mut Config| {
        target.server.public_url = Some("https://nora.example/base".into());
        target.repositories = config.repositories;
        target.circuit_breaker = config.circuit_breaker;
        target.audit = config.audit;
        target.curation = config.curation;
    };
    with_named_repositories(match storage {
        Some(storage) => create_test_context_with_storage_and_config(storage, customize),
        None => create_test_context_with_config(customize),
    })
    .await
}
async fn publish(
    format: Format,
    context: &TestContext,
    prefix: &str,
    name: &str,
    version: &str,
    data: &[u8],
    beta: bool,
) {
    if format == Format::Npm {
        let mut tags = json!({"latest":version});
        if beta {
            tags["beta"] = json!(version);
        }
        let document = json!({"name":name,"versions":{version:{"name":name,"version":version,"dist":{}}},"dist-tags":tags,
            "_attachments":{format.filename(name,version):{"data":base64::engine::general_purpose::STANDARD.encode(data)}}});
        assert_eq!(
            send(
                &context.app,
                Method::PUT,
                &format!("{prefix}{name}"),
                serde_json::to_vec(&document).unwrap()
            )
            .await
            .status(),
            StatusCode::CREATED
        );
    } else {
        let boundary = "group-publish";
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\":action\"\r\n\r\nfile_upload\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"version\"\r\n\r\n{version}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n", format.filename(name, version)).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        assert_eq!(
            send_with_headers(
                &context.app,
                Method::POST,
                prefix,
                vec![(
                    "content-type",
                    &format!("multipart/form-data; boundary={boundary}")
                )],
                body
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
}
async fn document(
    format: Format,
    context: &TestContext,
    prefix: &str,
    name: &str,
) -> (StatusCode, Value) {
    let response = send_with_headers(
        &context.app,
        Method::GET,
        &format.metadata(prefix, name),
        vec![(
            "accept",
            "application/vnd.pypi.simple.v1+json, application/json",
        )],
        "",
    )
    .await;
    let status = response.status();
    (
        status,
        serde_json::from_slice(&body_bytes(response).await).unwrap_or(Value::Null),
    )
}
fn url(format: Format, document: &Value) -> &str {
    if format == Format::Npm {
        document["versions"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["dist"]["tarball"]
            .as_str()
            .unwrap()
    } else {
        document["files"][0]["url"].as_str().unwrap()
    }
}
fn request_path(url: &str) -> String {
    reqwest::Url::parse(url)
        .unwrap()
        .path()
        .strip_prefix("/base")
        .unwrap()
        .to_string()
}
fn source_document(
    format: Format,
    uri: &str,
    name: &str,
    versions: &[(&str, &[u8])],
    metadata: bool,
) -> Value {
    if format == Format::Npm {
        let versions: serde_json::Map<String, Value> = versions.iter().map(|(version, data)| ((*version).into(), json!({"name":name,"version":version,"dist":{"tarball":format!("{uri}/files/{}", format.filename(name,version)),"integrity":format!("sha512-{}",base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(data)))}}))).collect();
        json!({"name":name,"versions":versions,"dist-tags":{}})
    } else {
        json!({"meta":{"api-version":"1.4"},"name":name,"versions":versions.iter().map(|(version,_)|version).collect::<Vec<_>>(),
            "files":versions.iter().map(|(version,data)| { let mut file=json!({"filename":format.filename(name,version),"url":format!("{uri}/files/{}",format.filename(name,version)),"hashes":{"sha256":hex::encode(sha2::Sha256::digest(data))},"size":data.len()});
                if metadata { file["core-metadata"] = json!({"sha256":hex::encode(sha2::Sha256::digest(b"core metadata"))}); } file }).collect::<Vec<_>>()})
    }
}
async fn mount(
    format: Format,
    server: &MockServer,
    name: &str,
    versions: &[(&str, &[u8])],
    metadata: bool,
) {
    Mock::given(method("GET"))
        .and(path(format.upstream_path(name)))
        .respond_with(ResponseTemplate::new(200).set_body_json(source_document(
            format,
            &server.uri(),
            name,
            versions,
            metadata,
        )))
        .mount(server)
        .await;
    for (version, data) in versions {
        Mock::given(method("GET"))
            .and(path(format!("/files/{}", format.filename(name, version))))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(*data))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/files/{}.metadata",
                format.filename(name, version)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"core metadata"))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn browser_group_views_never_refresh_mutate_or_infer_absence_from_an_uncached_proxy() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount(format, &a, "pkg", &[("1.0.0", b"body")], true).await;
        let (storage, counts) = crate::test_helpers::op_counting_storage();
        let context = context(
            configuration(format, Some((&a.uri(), &b.uri())), false, ""),
            Some(storage.clone()),
        )
        .await;
        assert_eq!(
            send(
                &context.app,
                Method::GET,
                "/api/ui/repositories/all/package/pkg",
                ""
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            a.received_requests().await.unwrap().is_empty()
                && b.received_requests().await.unwrap().is_empty()
        );
        assert_eq!(
            document(format, &context, "/a/", "pkg").await.0,
            StatusCode::OK
        );
        let mut keys = storage.list("").await.unwrap();
        keys.extend(storage.list_internal("repositories/").await.unwrap());
        keys.sort();
        keys.dedup();
        let mut before = Vec::new();
        for key in &keys {
            before.push((key.clone(), storage.get(key).await.unwrap()));
        }
        counts.reset();
        for uri in [
            "/api/ui/repositories/all/package/pkg",
            "/ui/repositories/all/package/pkg",
            "/api/ui/repositories/all",
        ] {
            assert_eq!(
                send(&context.app, Method::GET, uri, "").await.status(),
                StatusCode::OK,
                "{format:?}: {uri}"
            );
        }
        assert_eq!(counts.snapshot().get("put").copied().unwrap_or(0), 0);
        assert_eq!(counts.snapshot().get("delete").copied().unwrap_or(0), 0);
        let mut after = storage.list("").await.unwrap();
        after.extend(storage.list_internal("repositories/").await.unwrap());
        after.sort();
        after.dedup();
        assert_eq!(after, keys);
        for (key, bytes) in before {
            assert_eq!(storage.get(&key).await.unwrap(), bytes, "{key}");
        }
        assert_eq!(a.received_requests().await.unwrap().len(), 1);
        assert!(b.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn direct_fetch_and_group_cache_audits_record_the_selected_member_and_source() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount(format, &a, "pkg", &[("1.0.0", b"body")], false).await;
        let mut config = configuration(format, Some((&a.uri(), &b.uri())), false, "");
        config.audit.mode = crate::audit::AuditMode::File;
        let context = context(config, None).await;
        let (_, metadata) = document(format, &context, "/a/", "pkg").await;
        let response = send(
            &context.app,
            Method::GET,
            &request_path(url(format, &metadata)),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await.as_ref(), b"body");
        let member = context.state.repositories.get("a").unwrap();
        let body = if format == Format::Npm {
            format!("npm/pkg/tarballs/{}", format.filename("pkg", "1.0.0"))
        } else {
            format!("pypi/pkg/{}", format.filename("pkg", "1.0.0"))
        };
        let proof = if format == Format::Npm {
            crate::registry::npm::origin_key(&body)
        } else {
            crate::registry::pypi_ready_key_for_body(&body).unwrap()
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while member.storage.stat(&proof).await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (_, metadata) = document(format, &context, "/group/", "pkg").await;
        let response = send(
            &context.app,
            Method::GET,
            &request_path(url(format, &metadata)),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await.as_ref(), b"body");
        context.state.audit.shutdown().await;
        let entries: Vec<Value> = std::fs::read_to_string(context.state.audit.path())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| entry["action"] == "proxy_fetch" || entry["action"] == "cache_hit")
            .collect();
        for action in ["proxy_fetch", "cache_hit"] {
            assert!(
                entries.iter().any(|entry| entry["action"] == action
                    && entry["repository"] == "a"
                    && entry["source_id"] == member.source_id),
                "{format:?}: {entries:?}"
            );
        }
    }
}

#[tokio::test]
async fn direct_and_group_refusals_keep_the_actual_member_and_source() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        for proxy in [false, true] {
            let a = MockServer::start().await;
            let b = MockServer::start().await;
            let mut config = configuration(
                format,
                proxy
                    .then(|| (a.uri(), b.uri()))
                    .as_ref()
                    .map(|(a, b)| (a.as_str(), b.as_str())),
                false,
                "",
            );
            let policy = tempfile::tempdir().unwrap();
            let blocklist = policy.path().join("blocklist.json");
            config.audit.mode = crate::audit::AuditMode::File;
            if proxy {
                config.curation.internal_namespaces = vec!["pkg".into()];
            } else {
                std::fs::write(&blocklist,serde_json::to_vec(&json!({"version":1,"rules":[{"registry":format.name(),"name":"pkg","version":"*","reason":"blocked"}]})).unwrap()).unwrap();
                config.curation.mode = crate::config::CurationMode::Enforce;
                config.curation.blocklist_path = Some(blocklist.to_str().unwrap().into());
            }
            let context = context(config, None).await;
            if !proxy {
                publish(format, &context, "/a/", "pkg", "1.0.0", b"body", false).await;
            }
            for prefix in ["/a/", "/group/"] {
                let path = if proxy {
                    format!("{prefix}pkg/")
                } else if format == Format::Npm {
                    format!("{prefix}pkg/-/{}", format.filename("pkg", "1.0.0"))
                } else {
                    format!("{prefix}pkg/{}", format.filename("pkg", "1.0.0"))
                };
                let path = if proxy && format == Format::Npm {
                    path.trim_end_matches('/').to_owned()
                } else {
                    path
                };
                let response = send(&context.app, Method::GET, &path, "").await;
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "{format:?} {proxy}: {path}"
                );
            }
            if proxy {
                for prefix in ["/ui/repositories", "/api/ui/repositories"] {
                    let response = send(
                        &context.app,
                        Method::GET,
                        &format!("{prefix}/all/package/pkg"),
                        "",
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::FORBIDDEN);
                }
            }
            context.state.audit.shutdown().await;
            let member = context.state.repositories.get("a").unwrap();
            let entries: Vec<Value> = std::fs::read_to_string(context.state.audit.path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|entry| entry["action"] == "curation_block")
                .collect();
            assert_eq!(
                entries.len(),
                if proxy { 4 } else { 2 },
                "{format:?} {proxy}: {entries:?}"
            );
            assert!(
                entries
                    .iter()
                    .all(|entry| entry["repository"] == "a"
                        && entry["source_id"] == member.source_id),
                "{format:?} {proxy}: {entries:?}"
            );
            assert!(a.received_requests().await.unwrap().is_empty());
            assert!(b.received_requests().await.unwrap().is_empty());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn inaccessible_local_pypi_inventory_is_not_absence_for_protocol_or_browser_groups() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = crate::test_env::env_lock();
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::Storage::new_local(directory.path().to_str().unwrap());
    let context = context(configuration(Format::Pypi, None, false, ""), Some(storage)).await;
    publish(Format::Pypi, &context, "/a/", "pkg", "1.0.0", b"a", false).await;
    publish(Format::Pypi, &context, "/b/", "pkg", "2.0.0", b"b", false).await;
    let parent = directory.path().join("repositories/a/sources/local/pypi");
    for (path, mode) in [(parent.clone(), 0), (parent.join("pkg"), 0o400)] {
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let mut statuses = Vec::new();
        for uri in [
            "/group/pkg/",
            "/ui/repositories/all/package/pkg",
            "/api/ui/repositories/all/package/pkg",
        ] {
            statuses.push(send(&context.app, Method::GET, uri, "").await.status());
        }
        let inaccessible = std::fs::metadata(
            parent
                .join("pkg")
                .join(Format::Pypi.filename("pkg", "1.0.0")),
        )
        .is_err();
        std::fs::set_permissions(&path, permissions).unwrap();
        if inaccessible {
            assert_eq!(
                statuses,
                vec![StatusCode::SERVICE_UNAVAILABLE; 3],
                "{}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn transfer_time_integrity_refusals_survive_coalescing_and_keep_source_attribution() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Pypi;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    mount(format, &a, "pkg", &[("1.0.0", b"body")], false).await;
    let policy = tempfile::tempdir().unwrap();
    let allowlist = policy.path().join("allowlist.json");
    std::fs::write(
        &allowlist,
        r#"{"version":1,"entries":[{"registry":"pypi","name":"pkg","version":"1.0.0"}]}"#,
    )
    .unwrap();
    let mut config = configuration(format, Some((&a.uri(), &b.uri())), false, "");
    config.audit.mode = crate::audit::AuditMode::File;
    config.curation.mode = crate::config::CurationMode::Enforce;
    config.curation.require_integrity = true;
    config.curation.allowlist_path = Some(allowlist.to_str().unwrap().into());
    let context = context(config, None).await;
    for prefix in ["/a/", "/group/"] {
        let response = send(
            &context.app,
            Method::GET,
            &format!("{prefix}pkg/{}", format.filename("pkg", "1.0.0")),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-nora-rule"], "allowlist:integrity");
    }
    context.state.audit.shutdown().await;
    let member = context.state.repositories.get("a").unwrap();
    let entries: Vec<Value> = std::fs::read_to_string(context.state.audit.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["action"] == "curation_block")
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(entries
        .iter()
        .all(|entry| entry["repository"] == "a" && entry["source_id"] == member.source_id));
    let requests = a.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().starts_with("/files/"))
            .count(),
        2
    );
    assert!(b.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn group_and_browser_catalogues_include_metadata_only_packages_without_crawling_sources() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount(format, &a, "pkg", &[("1.0.0", b"body")], true).await;
        let context = context(
            configuration(format, Some((&a.uri(), &b.uri())), false, ""),
            None,
        )
        .await;
        for phase in [false, true] {
            if phase {
                assert_eq!(
                    document(format, &context, "/a/", "pkg").await.0,
                    StatusCode::OK
                );
            }
            for uri in ["/group/", "/api/ui/repositories/all"] {
                let response = send_with_headers(
                    &context.app,
                    Method::GET,
                    uri,
                    vec![(
                        "accept",
                        "application/vnd.pypi.simple.v1+json, application/json",
                    )],
                    "",
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
                assert_eq!(body.contains("\"pkg\""), phase, "{format:?}: {uri}: {body}");
            }
            assert_eq!(
                a.received_requests().await.unwrap().len(),
                usize::from(phase)
            );
            assert!(b.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn both_formats_select_the_first_package_without_merging_newer_versions_tags_or_files() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let context = context(configuration(format, None, false, ""), None).await;
        publish(format, &context, "/a/", "pkg", "0.9.0", b"older", false).await;
        publish(format, &context, "/b/", "pkg", "1.0.0", b"newer", true).await;
        let (status, package) = document(format, &context, "/group/", "pkg").await;
        assert_eq!(status, StatusCode::OK);
        assert!(url(format, &package).contains("/group/-/source/"));
        if format == Format::Npm {
            assert!(package["versions"].get("0.9.0").is_some());
            assert!(package["versions"].get("1.0.0").is_none());
            assert_eq!(
                send(&context.app, Method::GET, "/group/pkg/beta", "")
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
            let latest: Value = serde_json::from_slice(
                &body_bytes(send(&context.app, Method::GET, "/alias/pkg/latest", "").await).await,
            )
            .unwrap();
            assert_eq!(latest["version"], "0.9.0");
        } else {
            assert_eq!(package["files"].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            send(
                &context.app,
                Method::GET,
                &format.artifact("/group/", "pkg", "1.0.0"),
                ""
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let linked = request_path(url(format, &package));
        assert_eq!(
            body_bytes(send(&context.app, Method::GET, &linked, "").await)
                .await
                .as_ref(),
            b"older"
        );
        assert_eq!(
            body_bytes(
                send(
                    &context.app,
                    Method::GET,
                    &format.artifact("/b/", "pkg", "1.0.0"),
                    ""
                )
                .await
            )
            .await
            .as_ref(),
            b"newer"
        );
        let group = context
            .state
            .repositories
            .groups()
            .find(|group| group.name == "all")
            .unwrap();
        assert_eq!(group.definition.members, vec!["a", "b"]);
        assert!(context
            .state
            .storage
            .list(&"repositories/all/sources/".to_string())
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn current_priority_ownership_and_source_bindings_reject_old_or_forged_member_links() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let original = context(configuration(format, None, false, ""), None).await;
        publish(format, &original, "/a/", "pkg", "1.0.0", b"first", false).await;
        publish(format, &original, "/b/", "pkg", "1.0.0", b"second", false).await;
        let (_, package) = document(format, &original, "/group/", "pkg").await;
        let old = request_path(url(format, &package));
        let a = crate::repositories::groups::binding(original.state.repositories.get("a").unwrap());
        let b = crate::repositories::groups::binding(original.state.repositories.get("b").unwrap());
        assert_eq!(
            send(&original.app, Method::GET, &old.replace(&a, &b), "")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        for (reversed, owner) in [(true, ""), (false, "{\"pkg\"=\"b\"}")] {
            let changed = context(
                configuration(format, None, reversed, owner),
                Some(original.state.storage.clone()),
            )
            .await;
            assert_eq!(
                send(&changed.app, Method::GET, &old, "").await.status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                body_bytes(
                    send(
                        &changed.app,
                        Method::GET,
                        &old.replacen("/group/", "/other/", 1),
                        ""
                    )
                    .await
                )
                .await
                .as_ref(),
                b"first"
            );
            assert_eq!(
                body_bytes(
                    send(
                        &changed.app,
                        Method::GET,
                        &format.artifact("/a/", "pkg", "1.0.0"),
                        ""
                    )
                    .await
                )
                .await
                .as_ref(),
                b"first"
            );
        }
    }
}

#[tokio::test]
async fn only_confirmed_absence_falls_through_and_selected_failures_never_query_the_next_source() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        for code in [404, 401, 403, 503, 200, 408] {
            let a = MockServer::start().await;
            let b = MockServer::start().await;
            let response = match code {
                200 => ResponseTemplate::new(200).set_body_string("{malformed"),
                408 => ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
                _ => ResponseTemplate::new(code),
            };
            Mock::given(method("GET"))
                .and(path(format.upstream_path("pkg")))
                .respond_with(response)
                .mount(&a)
                .await;
            mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
            let context = context(
                configuration(format, Some((&a.uri(), &b.uri())), false, ""),
                None,
            )
            .await;
            let (status, _) = document(format, &context, "/group/", "pkg").await;
            if code == 404 {
                assert_eq!(status, StatusCode::OK);
                assert!(!b.received_requests().await.unwrap().is_empty());
            } else {
                assert!(status.is_server_error(), "{format:?}: {code} -> {status}");
                assert!(b.received_requests().await.unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn expired_confirmed_absence_retires_old_positive_metadata_instead_of_becoming_an_outage() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount(format, &a, "pkg", &[("1.0.0", b"private")], true).await;
        mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
        let context = context(
            configuration(format, Some((&a.uri(), &b.uri())), false, ""),
            None,
        )
        .await;
        assert_eq!(
            document(format, &context, "/group/", "pkg").await.0,
            StatusCode::OK
        );
        a.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&a)
            .await;
        let (status, package) = document(format, &context, "/group/", "pkg").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            url(format, &package).contains(&crate::repositories::groups::binding(
                context.state.repositories.get("b").unwrap()
            ))
        );
        if format == Format::Npm {
            let a = context.state.repositories.get("a").unwrap();
            let key = crate::registry::npm::upstream_packument_key("pkg");
            assert!(a.storage.stat(&key).await.is_some());
            assert!(a
                .storage
                .stat(&crate::registry::npm::origin_key(&key))
                .await
                .is_none());
        }
        a.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&a)
            .await;
        b.reset().await;
        mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
        assert!(document(format, &context, "/group/", "pkg")
            .await
            .0
            .is_server_error());
        assert!(b.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_fresh_persisted_pypi_negative_falls_through_without_probing_the_source_again() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Pypi;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&a)
        .await;
    mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
    let original = context(
        configuration(format, Some((&a.uri(), &b.uri())), false, ""),
        None,
    )
    .await;
    assert_eq!(
        document(format, &original, "/group/", "pkg").await.0,
        StatusCode::OK
    );
    a.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;
    let mut config = configuration(format, Some((&a.uri(), &b.uri())), false, "");
    for repository in config.repositories.values_mut() {
        if let crate::config::repositories::RepositoryConfig::Proxy(proxy) = repository {
            proxy.metadata_ttl = 300;
        }
    }
    let restarted = context(config, Some(original.state.storage.clone())).await;
    assert_eq!(
        document(format, &restarted, "/alias/", "pkg").await.0,
        StatusCode::OK
    );
    assert!(a.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unreadable_member_storage_is_failure_not_an_empty_package() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let (storage, counts) = crate::test_helpers::op_counting_storage();
        let context = context(configuration(format, None, false, ""), Some(storage)).await;
        publish(format, &context, "/b/", "pkg", "1.0.0", b"public", false).await;
        counts.fail_reads_to(Some("repositories/a/sources/local/"));
        assert_eq!(
            document(format, &context, "/group/", "pkg").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

#[tokio::test]
async fn corrupt_selected_metadata_or_file_bytes_do_not_read_a_lower_priority_copy() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let backend = Arc::new(crate::storage::ObjectStorage::in_memory());
        let storage = crate::Storage::from_backend(backend.clone());
        let context = context(configuration(format, None, false, ""), Some(storage)).await;
        publish(format, &context, "/a/", "pkg", "1.0.0", b"private", false).await;
        publish(format, &context, "/b/", "pkg", "1.0.0", b"public", false).await;
        let key = if format == Format::Npm {
            "npm/pkg/versions/1.0.0.json".to_string()
        } else {
            format!("pypi/pkg/{}", format.filename("pkg", "1.0.0"))
        };
        if format == Format::Npm {
            context
                .state
                .repositories
                .get("a")
                .unwrap()
                .storage
                .delete("npm/pkg/metadata.json")
                .await
                .unwrap();
        }
        crate::storage::StorageBackend::put(
            backend.as_ref(),
            &format!("repositories/a/sources/local/{key}"),
            b"corrupt",
            &"00".repeat(32),
        )
        .await
        .unwrap();
        if format == Format::Npm {
            assert_eq!(
                document(format, &context, "/group/", "pkg").await.0,
                StatusCode::SERVICE_UNAVAILABLE
            );
        } else {
            let response = send(
                &context.app,
                Method::GET,
                &format.artifact("/group/", "pkg", "1.0.0"),
                "",
            )
            .await;
            assert!(axum::body::to_bytes(response.into_body(), 1024)
                .await
                .is_err());
        }
    }
}

#[tokio::test]
async fn direct_npm_retained_tarballs_do_not_require_metadata_but_groups_still_require_selection() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Npm;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    mount(format, &a, "pkg", &[("1.0.0", b"private")], true).await;
    mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
    let mut config = configuration(format, Some((&a.uri(), &b.uri())), false, "");
    for repository in config.repositories.values_mut() {
        if let crate::config::repositories::RepositoryConfig::Proxy(proxy) = repository {
            proxy.metadata_ttl = 300;
        }
    }
    let context = context(config, None).await;
    let direct = format.artifact("/a/", "pkg", "1.0.0");
    assert_eq!(
        body_bytes(send(&context.app, Method::GET, &direct, "").await)
            .await
            .as_ref(),
        b"private"
    );
    context.state.background_tasks.close();
    context.state.background_tasks.wait().await;
    let member = context.state.repositories.get("a").unwrap();
    let key = crate::registry::npm::upstream_packument_key("pkg");
    member.storage.delete(&key).await.unwrap();
    member
        .storage
        .delete(&crate::registry::npm::origin_key(&key))
        .await
        .unwrap();
    a.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;
    let response = send(&context.app, Method::GET, &direct, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), b"private");
    assert!(send(
        &context.app,
        Method::GET,
        &format.artifact("/group/", "pkg", "1.0.0"),
        ""
    )
    .await
    .status()
    .is_server_error());
    assert!(b.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_modified_named_body_never_keeps_old_validators_when_the_new_validator_write_fails() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Npm;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"one\"")
                .set_body_json(source_document(
                    format,
                    &a.uri(),
                    "pkg",
                    &[("1.0.0", b"first")],
                    true,
                )),
        )
        .mount(&a)
        .await;
    let (storage, counts) = crate::test_helpers::op_counting_storage();
    let context = context(
        configuration(format, Some((&a.uri(), &b.uri())), false, ""),
        Some(storage),
    )
    .await;
    assert!(document(format, &context, "/a/", "pkg").await.1["versions"]
        .get("1.0.0")
        .is_some());
    let member = context.state.repositories.get("a").unwrap();
    let key = crate::registry::npm::upstream_packument_key("pkg");
    assert_eq!(
        crate::registry::read_validators(&member.storage, &key)
            .await
            .unwrap()
            .etag
            .as_deref(),
        Some("\"one\"")
    );
    counts.fail_writes_to(Some(&format!(
        "repositories/a/sources/{}/{}",
        member.source_id,
        crate::registry::validators_key(&key)
    )));
    a.reset().await;
    Mock::given(method("GET"))
        .and(path("/pkg"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"two\"")
                .set_body_json(source_document(
                    format,
                    &a.uri(),
                    "pkg",
                    &[("2.0.0", b"second")],
                    true,
                )),
        )
        .mount(&a)
        .await;
    assert!(document(format, &context, "/a/", "pkg").await.1["versions"]
        .get("2.0.0")
        .is_some());
    let stored: Value = serde_json::from_slice(&member.storage.get(&key).await.unwrap()).unwrap();
    assert!(stored["versions"].get("2.0.0").is_some());
    assert!(crate::registry::read_validators(&member.storage, &key)
        .await
        .is_none());
}

#[tokio::test]
async fn typed_npm_refresh_failures_keep_the_circuit_open_retry_response() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Npm;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;
    mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
    let mut config = configuration(format, Some((&a.uri(), &b.uri())), false, "");
    config.circuit_breaker.enabled = true;
    config.circuit_breaker.failure_threshold = 1;
    config.circuit_breaker.reset_timeout = 3600;
    let context = context(config, None).await;
    assert!(document(format, &context, "/a/", "pkg")
        .await
        .0
        .is_server_error());
    for prefix in ["/a/", "/group/"] {
        let response = send(
            &context.app,
            Method::GET,
            &format.metadata(prefix, "pkg"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "30");
        assert!(!String::from_utf8_lossy(&body_bytes(response).await).contains(&a.uri()));
    }
    assert!(b.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn explicit_owners_are_exclusive_even_when_absent_or_down() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        for code in [404, 503] {
            let a = MockServer::start().await;
            let b = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(code))
                .mount(&a)
                .await;
            mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
            let context = context(
                configuration(format, Some((&a.uri(), &b.uri())), false, "{\"pkg\"=\"a\"}"),
                None,
            )
            .await;
            let (status, _) = document(format, &context, "/group/", "pkg").await;
            assert_eq!(
                status,
                if code == 404 {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_GATEWAY
                }
            );
            assert!(b.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn positive_empty_packages_quarantine_and_offline_filtering_are_not_absence() {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount(format, &a, "pkg", &[], true).await;
        mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
        let context = context(
            configuration(format, Some((&a.uri(), &b.uri())), false, ""),
            None,
        )
        .await;
        assert_eq!(
            document(format, &context, "/group/", "pkg").await.0,
            StatusCode::OK
        );
        assert!(b.received_requests().await.unwrap().is_empty());
        a.reset().await;
        let mut package = source_document(format, &a.uri(), "pkg", &[("1.0.0", b"private")], true);
        if format == Format::Pypi {
            package["project-status"] = json!({"status":"quarantined","reason":"held"});
        }
        Mock::given(method("GET"))
            .and(path(format.upstream_path("pkg")))
            .respond_with(ResponseTemplate::new(200).set_body_json(package))
            .mount(&a)
            .await;
        let (status, package) = document(format, &context, "/alias/", "pkg").await;
        assert_eq!(status, StatusCode::OK);
        if format == Format::Pypi {
            assert!(package["files"].as_array().unwrap().is_empty());
            assert_eq!(
                send(
                    &context.app,
                    Method::GET,
                    &format.artifact("/group/", "pkg", "1.0.0"),
                    ""
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
        a.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&a)
            .await;
        assert_eq!(
            document(format, &context, "/group/", "pkg").await.0,
            StatusCode::OK
        );
        assert!(b.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn group_sidecars_stay_with_the_selected_source_and_survive_only_valid_bindings() {
    let _lock = crate::test_env::env_lock();
    let format = Format::Pypi;
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    mount(format, &a, "pkg", &[("1.0.0", b"private")], true).await;
    mount(format, &b, "pkg", &[("1.0.0", b"public")], true).await;
    let context = context(
        configuration(format, Some((&a.uri(), &b.uri())), false, ""),
        None,
    )
    .await;
    let (_, package) = document(format, &context, "/group/", "pkg").await;
    let sidecar = format!("{}.metadata", request_path(url(format, &package)));
    assert_eq!(
        body_bytes(send(&context.app, Method::GET, &sidecar, "").await)
            .await
            .as_ref(),
        b"core metadata"
    );
    assert!(b.received_requests().await.unwrap().is_empty());
    a.reset().await;
    mount(format, &a, "pkg", &[("1.0.0", b"private")], false).await;
    assert_eq!(
        send(&context.app, Method::GET, &sidecar, "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(b.received_requests().await.unwrap().is_empty());
    let changed = self::context(
        configuration(format, Some((&a.uri(), &b.uri())), true, ""),
        Some(context.state.storage.clone()),
    )
    .await;
    assert_eq!(
        send(&changed.app, Method::GET, &sidecar, "").await.status(),
        StatusCode::NOT_FOUND
    );
    let new_a = MockServer::start().await;
    let changed = self::context(
        configuration(format, Some((&new_a.uri(), &b.uri())), false, ""),
        Some(context.state.storage.clone()),
    )
    .await;
    assert_eq!(
        send(&changed.app, Method::GET, &sidecar, "").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn group_write_and_audit_failures_are_explicit_and_ownership_normalization_is_format_specific(
) {
    let _lock = crate::test_env::env_lock();
    for format in [Format::Npm, Format::Pypi] {
        let owner = if format == Format::Pypi {
            "{\"corp-*\"=\"a\",\"corp*\"=\"b\"}"
        } else {
            "{\"@Scope/*\"=\"b\"}"
        };
        let context = context(configuration(format, None, false, owner), None).await;
        if format == Format::Pypi {
            publish(
                format, &context, "/b/", "corp-foo", "1.0.0", b"lower", false,
            )
            .await;
            assert_eq!(
                document(format, &context, "/group/", "Corp_Foo").await.0,
                StatusCode::NOT_FOUND
            );
            publish(
                format,
                &context,
                "/b/",
                "corporate",
                "1.0.0",
                b"company",
                false,
            )
            .await;
            assert_eq!(
                document(format, &context, "/group/", "corporate").await.0,
                StatusCode::OK
            );
        } else {
            publish(
                format,
                &context,
                "/a/",
                "@scope/pkg",
                "1.0.0",
                b"lower",
                false,
            )
            .await;
            publish(
                format,
                &context,
                "/b/",
                "@Scope/pkg",
                "1.0.0",
                b"upper",
                false,
            )
            .await;
            assert_eq!(
                body_bytes(
                    send(
                        &context.app,
                        Method::GET,
                        &format.artifact("/group/", "@scope/pkg", "1.0.0"),
                        ""
                    )
                    .await
                )
                .await
                .as_ref(),
                b"lower"
            );
            assert_eq!(
                body_bytes(
                    send(
                        &context.app,
                        Method::GET,
                        &format.artifact("/group/", "@Scope/pkg", "1.0.0"),
                        ""
                    )
                    .await
                )
                .await
                .as_ref(),
                b"upper"
            );
            let audit = send(
                &context.app,
                Method::POST,
                "/group/-/npm/v1/security/advisories/bulk",
                "{}",
            )
            .await;
            assert_eq!(audit.status(), StatusCode::NOT_IMPLEMENTED);
            assert!(serde_json::from_slice::<Value>(&body_bytes(audit).await)
                .unwrap()
                .get("error")
                .is_some());
        }
        assert_eq!(
            send(
                &context.app,
                if format == Format::Npm {
                    Method::PUT
                } else {
                    Method::POST
                },
                &format.metadata("/group/", "pkg"),
                "{}"
            )
            .await
            .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}
