// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Protocol rendering follows a single package selection, never a version-by-version merge.

use super::*;
use crate::config::repositories::RepositoryConfig;
use crate::repositories::groups::{self, Group, Lookup};

#[derive(Clone)]
struct Mount {
    group: Arc<Group>,
    prefix: Arc<str>,
}

pub(crate) fn routes_at(prefix: &str, group: Arc<Group>) -> Router<AppState> {
    Router::new()
        .route(prefix, get(index))
        .route(
            &format!("{prefix}{{*path}}"),
            get(handle)
                .post(post)
                .fallback(|| async { method_not_allowed("GET, HEAD") }),
        )
        .layer(Extension(Mount {
            group,
            prefix: Arc::from(prefix),
        }))
}

async fn post(Path(path): Path<String>) -> Response {
    if matches!(
        path.as_str(),
        "-/npm/v1/security/advisories/bulk"
            | "-/npm/v1/security/audits/quick"
            | "-/npm/v1/security/audits"
    ) {
        audit_refused(
            StatusCode::NOT_IMPLEMENTED,
            "npm audit is not supported for repository groups",
        )
    } else {
        method_not_allowed("GET, HEAD")
    }
}

async fn index(State(root): State<AppState>, Extension(mount): Extension<Mount>) -> Response {
    let mut packages = std::collections::BTreeSet::new();
    for member in &mount.group.definition.members {
        let Some(repository) = root.repositories.get(member) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let state = repository.state(&root);
        match known_packages(&state).await {
            Ok(known) => packages.extend(known),
            Err(status) => return status.into_response(),
        };
    }
    axum::Json(serde_json::json!({"name":mount.group.name,"packages":packages})).into_response()
}

async fn lookup(state: &AppState, name: &str) -> Lookup<named::Package, Box<Response>> {
    let Some(repository) = &state.repository else {
        return Lookup::Failure(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
    };
    let loaded = match repository.definition {
        RepositoryConfig::Local(_) => match published_state(&state.storage, name).await {
            Published::Yes(versions) => load_published_packument(state, name, &versions)
                .await
                .map(|body| Some(named::Package { body, stale: false }))
                .map_err(|status| Box::new(status.into_response())),
            Published::No => Ok(None),
            Published::Unknown => {
                let key = published_packument_key(name);
                match state.storage.get(&key).await {
                    Ok(body) if is_assembled_by_nora(&state.storage, &key, &body).await => {
                        Ok(Some(named::Package { body, stale: false }))
                    }
                    _ => Err(Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response())),
                }
            }
        },
        RepositoryConfig::Proxy(_) => {
            if let Some(response) = crate::curation::check_namespace_isolation(
                &state.curation().curation_engine,
                crate::curation::RegistryType::Npm,
                name,
            ) {
                return Lookup::Failure(Box::new(crate::audit::attribute_refusal(state, response)));
            }
            load_upstream_metadata(state, name, name, &upstream_packument_key(name), true).await
        }
        RepositoryConfig::Group(_) => {
            Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))
        }
    };
    match loaded {
        Ok(Some(package)) => {
            let valid = serde_json::from_slice::<serde_json::Value>(&package.body)
                .ok()
                .is_some_and(|document| {
                    document.get("name").and_then(|name| name.as_str()) == Some(name)
                        && document
                            .get("versions")
                            .is_some_and(|versions| versions.is_object())
                });
            if valid {
                Lookup::Present(package)
            } else {
                Lookup::Failure(Box::new(StatusCode::BAD_GATEWAY.into_response()))
            }
        }
        Ok(None) => Lookup::Absent,
        Err(response) => {
            Lookup::Failure(Box::new(crate::audit::attribute_refusal(state, *response)))
        }
    }
}

async fn handle(
    State(root): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    let (expected, path) = if let Some(bound) = path.strip_prefix("-/source/") {
        let Some((token, path)) = bound.split_once('/') else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (Some(token), path)
    } else {
        (None, path.as_str())
    };
    if path == "-/whoami" && expected.is_none() {
        return handle_whoami(&user).await;
    }
    let github = github_packages_tarball(path);
    let artifact = github.is_some() || path.contains("/-/");
    let target = classify_metadata_path(path);
    let name = if let Some((name, _)) = github {
        name
    } else if artifact {
        path.split("/-/").next().unwrap_or("")
    } else {
        match target {
            MetadataPath::Packument { name } | MetadataPath::VersionManifest { name, .. } => name,
            MetadataPath::Other => match dist_tags_package(path) {
                Some(name) => name,
                None => return StatusCode::NOT_FOUND.into_response(),
            },
        }
    };
    if !valid_package_name(name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let selection = match groups::resolve(
        &root.repositories,
        &mount.group,
        name,
        expected,
        |repository| {
            let mut state = repository.state(&root);
            crate::registry::transport::begin_metadata(&mut state);
            async move { lookup(&state, name).await }
        },
    )
    .await
    {
        Ok(Some(selection)) => selection,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return groups::failure(error),
    };
    let mut state = selection.repository.state(&root);
    let prefix = groups::bound_prefix(&mount.prefix, &selection.binding);
    named::bind(&mut state, named::Mount(Arc::from(prefix.as_str())));
    if artifact {
        let response = handle_request(
            State(state.clone()),
            Extension(named::Mount(Arc::from(prefix))),
            headers,
            Path(path.to_owned()),
            Extension(user),
            Some(Extension(selection.package)),
        )
        .await;
        return crate::audit::attribute_refusal(&state, response);
    }
    let mut package = selection.package;
    match target {
        MetadataPath::Packument { .. } => named::package_response(&state, &headers, package, name),
        _ => {
            let Ok(document) = serde_json::from_slice::<serde_json::Value>(&package.body) else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            let selected = match target {
                MetadataPath::VersionManifest { spec, .. } => {
                    let version = document
                        .get("dist-tags")
                        .and_then(|tags| tags.get(spec))
                        .and_then(|tag| tag.as_str())
                        .unwrap_or(spec);
                    document
                        .get("versions")
                        .and_then(|versions| versions.get(version))
                }
                MetadataPath::Other => document.get("dist-tags"),
                _ => unreachable!(),
            };
            let Some(selected) = selected else {
                return StatusCode::NOT_FOUND.into_response();
            };
            package.body = Bytes::from(serde_json::to_vec(selected).unwrap_or_default());
            let mut response = named::package_response(&state, &HeaderMap::new(), package, name);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        }
    }
}
