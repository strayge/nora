// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Empty/quarantined/offline project pages still select their source package.

use super::*;
use crate::config::repositories::RepositoryConfig;
use crate::repositories::groups::{self, Group, Lookup, Selection};

#[derive(Clone)]
struct Mount {
    group: Arc<Group>,
    prefix: String,
}
enum Package {
    Local,
    Proxy(named::View),
}

pub(crate) fn routes_at(prefix: &str, group: Arc<Group>) -> Router<AppState> {
    Router::new()
        .route(
            prefix,
            get(index).fallback(|| async { method_not_allowed("GET, HEAD") }),
        )
        .route(&format!("{prefix}{{name}}/"), get(project))
        .route(&format!("{prefix}{{name}}/{{filename}}"), get(file))
        .route(
            &format!("{prefix}-/source/{{token}}/{{name}}/{{filename}}"),
            get(bound_file),
        )
        .layer(Extension(Mount {
            group,
            prefix: prefix.into(),
        }))
}

async fn index(
    State(root): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Response {
    let mut names = std::collections::BTreeSet::new();
    for member in &mount.group.definition.members {
        let Some(repository) = root.repositories.get(member) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let state = repository.state(&root);
        if named::is_proxy(&state) {
            match named::known_projects(&state).await {
                Ok(known) => names.extend(known),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        } else {
            let keys = match state.storage.list("pypi/").await {
                Ok(keys) => keys,
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            names.extend(
                keys.iter()
                    .filter_map(|key| key.strip_prefix("pypi/")?.split('/').next())
                    .filter(|name| model::valid_name(name))
                    .map(str::to_owned),
            );
        }
    }
    package_list_response(
        &root,
        &PypiMount(mount.prefix),
        &headers,
        names.into_iter().collect(),
    )
}

async fn lookup(state: &AppState, name: &str) -> Lookup<Package, Box<Response>> {
    let Some(repository) = &state.repository else {
        return Lookup::Failure(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
    };
    match repository.definition {
        RepositoryConfig::Local(_) => {
            let prefix = format!("pypi/{name}/");
            match state.storage.list_with_meta(&prefix).await {
                Ok(files)
                    if files.iter().any(|(key, _)| {
                        key.strip_prefix(&prefix)
                            .is_some_and(is_valid_pypi_filename)
                    }) =>
                {
                    Lookup::Present(Package::Local)
                }
                Ok(_) => Lookup::Absent,
                Err(_) => {
                    Lookup::Failure(Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))
                }
            }
        }
        RepositoryConfig::Proxy(_) => {
            if let Some(response) = crate::curation::check_namespace_isolation(
                &state.curation().curation_engine,
                crate::curation::RegistryType::PyPI,
                name,
            ) {
                return Lookup::Failure(Box::new(response));
            }
            match named::ensure(state, name).await {
                Ok(view) if view.snapshot.project.is_some() => {
                    Lookup::Present(Package::Proxy(view))
                }
                Ok(_) | Err(crate::registry::ProxyError::NotFound) => Lookup::Absent,
                Err(error) => Lookup::Failure(Box::new(named::failure(error))),
            }
        }
        RepositoryConfig::Group(_) => {
            Lookup::Failure(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))
        }
    }
}

async fn select(
    root: &AppState,
    mount: &Mount,
    name: &str,
    token: Option<&str>,
) -> Result<Option<Selection<Package>>, groups::ResolveError<Box<Response>>> {
    groups::resolve(
        &root.repositories,
        &mount.group,
        name,
        token,
        |repository| {
            let mut state = repository.state(root);
            crate::registry::transport::begin_metadata(&mut state);
            async move { lookup(&state, name).await }
        },
    )
    .await
}

async fn project(
    State(root): State<AppState>,
    Extension(mount): Extension<Mount>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let name = normalize_name(&name);
    if !model::valid_name(&name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let selection = match select(&root, &mount, &name, None).await {
        Ok(Some(selection)) => selection,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return groups::failure(error),
    };
    let state = selection.repository.state(&root);
    let mount = PypiMount(groups::bound_prefix(&mount.prefix, &selection.binding));
    match selection.package {
        Package::Local => {
            package_versions(State(state), Extension(mount), Path(name), headers).await
        }
        Package::Proxy(view) => {
            named::project_view(
                &state,
                &name,
                &index_base_url(&state, &mount),
                &headers,
                view,
            )
            .await
        }
    }
}

async fn file(
    State(root): State<AppState>,
    Extension(mount): Extension<Mount>,
    method: Method,
    Path((name, filename)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    download(root, mount, method, name, filename, headers, None).await
}
async fn bound_file(
    State(root): State<AppState>,
    Extension(mount): Extension<Mount>,
    method: Method,
    Path((token, name, filename)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    download(root, mount, method, name, filename, headers, Some(token)).await
}
async fn download(
    root: AppState,
    mount: Mount,
    method: Method,
    name: String,
    filename: String,
    headers: HeaderMap,
    token: Option<String>,
) -> Response {
    let name = normalize_name(&name);
    if !model::valid_name(&name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let selection = match select(&root, &mount, &name, token.as_deref()).await {
        Ok(Some(selection)) => selection,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return groups::failure(error),
    };
    let prepared = match selection.package {
        Package::Local => None,
        Package::Proxy(view) => Some(Extension(view)),
    };
    download_file(
        State(selection.repository.state(&root)),
        method,
        headers,
        Path((name, filename)),
        prepared,
    )
    .await
}
