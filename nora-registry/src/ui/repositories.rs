// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Repository identities are explicit; a group's inventory never merges selected members.

use super::{
    components, extract_lang_from_headers, resolve_base_url, templates::encode_uri_component,
};
use crate::config::repositories::{RepositoryConfig, RepositoryFormat};
use crate::repositories::{groups, Repository};
use crate::{registry_type::RegistryType, AppState};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt::Write, sync::Arc};

mod artifacts;

#[cfg(test)]
mod tests;

#[derive(Default, Deserialize)]
struct BrowseQuery {
    format: Option<String>,
    q: Option<String>,
    page: Option<usize>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Summary {
    name: String,
    format: &'static str,
    kind: &'static str,
    paths: Vec<String>,
    members: Vec<String>,
    save_files: Option<bool>,
    stored_packages: Option<usize>,
    stored_artifacts: Option<usize>,
    stored_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Listing {
    repository: Summary,
    packages: Vec<String>,
    total: usize,
    page: usize,
    limit: usize,
}

#[derive(Serialize)]
struct Commands {
    endpoint: String,
    install: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    publish: Option<String>,
}

#[derive(Serialize)]
struct StoredVersion {
    version: String,
    size: u64,
    updated: String,
}

#[derive(Serialize)]
struct Package {
    repository: Summary,
    name: String,
    source_repository: String,
    commands: Vec<Commands>,
    stored_versions: Vec<StoredVersion>,
    artifacts: artifacts::Inventory,
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    releases: Option<Vec<Release>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    residual_files: Option<usize>,
}

#[derive(Serialize)]
struct Release {
    version: String,
    files: usize,
    size: u64,
}

#[derive(Default, Deserialize)]
struct PackageQuery {
    retry_version: Option<String>,
    version: Option<String>,
    page: Option<usize>,
    limit: Option<usize>,
    other_page: Option<usize>,
}

pub(super) fn selection_required() -> Response {
    (StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"repository_required","message":"Choose a named repository; npm and PyPI have no default repository","repositories":"/api/ui/repositories"}))).into_response()
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/repositories", get(index))
        .route(
            "/ui/repositories/",
            get(|| async { axum::response::Redirect::to("/ui/repositories") }),
        )
        .route("/ui/repositories/{repository}", get(repository))
        .route(
            "/ui/repositories/{repository}/package/{*name}",
            get(package),
        )
        .route("/api/ui/repositories", get(api_index))
        .route("/api/ui/repositories/{repository}", get(api_repository))
        .route(
            "/api/ui/repositories/{repository}/package/{*name}",
            get(api_package),
        )
}

fn registry(format: RepositoryFormat) -> RegistryType {
    match format {
        RepositoryFormat::Npm => RegistryType::Npm,
        RepositoryFormat::Pypi => RegistryType::PyPI,
    }
}

async fn summary(root: &AppState, name: &str) -> Option<Summary> {
    let definition = root.config.repositories.get(name)?;
    let format = registry(definition.format());
    if !root.enabled_registries.contains(&format) {
        return None;
    }
    let (kind, members, save_files) = match definition {
        RepositoryConfig::Local(_) => ("local", vec![], None),
        RepositoryConfig::Proxy(proxy) => ("proxy", vec![], Some(proxy.save_files)),
        RepositoryConfig::Group(group) => ("group", group.members.clone(), None),
    };
    let inventory = if let Some(member) = root.repositories.get(name) {
        Some(
            member
                .state(root)
                .repo_index
                .get(format.as_str(), &member.storage)
                .await,
        )
    } else {
        None
    };
    Some(Summary {
        name: name.into(),
        format: format.as_str(),
        kind,
        paths: definition.paths().to_vec(),
        members,
        save_files,
        stored_packages: inventory
            .as_ref()
            .map(|items| items.iter().filter(|item| item.versions > 0).count()),
        stored_artifacts: inventory
            .as_ref()
            .map(|items| items.iter().map(|item| item.versions).sum()),
        stored_bytes: inventory
            .as_ref()
            .map(|items| items.iter().map(|item| item.size).sum()),
    })
}

async fn summaries(root: &AppState, query: &BrowseQuery) -> Result<Vec<Summary>, Box<Response>> {
    if query
        .format
        .as_deref()
        .is_some_and(|format| !matches!(format, "npm" | "pypi"))
    {
        return Err(Box::new(StatusCode::BAD_REQUEST.into_response()));
    }
    let mut entries = Vec::new();
    for name in root.config.repositories.keys() {
        if let Some(entry) = summary(root, name).await {
            if query
                .format
                .as_deref()
                .is_none_or(|format| format == entry.format)
            {
                entries.push(entry);
            }
        }
    }
    Ok(entries)
}

async fn member_names(
    root: &AppState,
    member: &Arc<Repository>,
) -> Result<Vec<String>, Box<Response>> {
    let state = member.state(root);
    match member.definition.format() {
        RepositoryFormat::Npm => crate::registry::npm::known_packages(&state).await,
        RepositoryFormat::Pypi => crate::registry::known_pypi_packages(&state).await,
    }
    .map_err(|status| Box::new(status.into_response()))
}

async fn listing(
    root: &AppState,
    name: &str,
    query: &BrowseQuery,
) -> Result<Listing, Box<Response>> {
    let repository = summary(root, name)
        .await
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let mut names = BTreeSet::new();
    if let Some(member) = root.repositories.get(name) {
        names.extend(member_names(root, member).await?);
    } else {
        for member in &repository.members {
            let member = root
                .repositories
                .get(member)
                .ok_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
            names.extend(member_names(root, member).await?);
        }
    }
    let q = query.q.as_deref().unwrap_or("").to_lowercase();
    let names: Vec<_> = names
        .into_iter()
        .filter(|name| name.to_lowercase().contains(&q))
        .collect();
    let total = names.len();
    let page = query.page.unwrap_or(1).max(1);
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let packages = names
        .into_iter()
        .skip(page.saturating_sub(1).saturating_mul(limit))
        .take(limit)
        .collect();
    Ok(Listing {
        repository,
        packages,
        total,
        page,
        limit,
    })
}

async fn selected(
    root: &AppState,
    repository: &str,
    name: &str,
) -> Result<Arc<Repository>, Box<Response>> {
    if let Some(member) = root.repositories.get(repository) {
        return Ok(Arc::clone(member));
    }
    let group = root
        .repositories
        .groups()
        .find(|group| group.name == repository)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let format = group.definition.format;
    let selection = groups::resolve(&root.repositories, group, name, None, |member| {
        let state = member.state(root);
        async move {
            let presence = match format {
                RepositoryFormat::Npm => {
                    crate::registry::npm::stored_package_presence(&state, name).await
                }
                RepositoryFormat::Pypi => {
                    crate::registry::stored_pypi_package_presence(&state, name).await
                }
            };
            match presence {
                groups::Lookup::Failure(response) => groups::Lookup::Failure(Box::new(
                    crate::audit::attribute_refusal(&state, *response),
                )),
                presence => presence,
            }
        }
    })
    .await
    .map_err(|error| Box::new(groups::failure(error)))?;
    selection
        .map(|selection| selection.repository)
        .ok_or_else(|| Box::new(StatusCode::NOT_FOUND.into_response()))
}

fn shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn commands(root: &AppState, repository: &Summary, name: &str) -> Vec<Commands> {
    repository
        .paths
        .iter()
        .map(|path| {
            let endpoint = format!("{}{}", resolve_base_url(root).trim_end_matches('/'), path);
            let url = shell_arg(&endpoint);
            let install = if repository.format == "npm" {
                format!("npm install --registry {url} -- {}", shell_arg(name))
            } else {
                format!(
                    "python -m pip install --index-url {url} -- {}",
                    shell_arg(name)
                )
            };
            let publish = (repository.kind == "local").then(|| {
                if repository.format == "npm" {
                    format!("npm publish --registry {url}")
                } else {
                    format!("python -m twine upload --repository-url {url} dist/*")
                }
            });
            Commands {
                endpoint,
                install,
                publish,
            }
        })
        .collect()
}

async fn package_data(
    root: &AppState,
    repository: &str,
    name: &str,
    query: &PackageQuery,
) -> Result<Package, Box<Response>> {
    let repository = summary(root, repository)
        .await
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let name = if repository.format == "npm" {
        if !crate::registry::npm::valid_package_name(name) {
            return Err(Box::new(StatusCode::BAD_REQUEST.into_response()));
        }
        name.to_owned()
    } else {
        crate::registry::canonical_pypi_project(name)
            .ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?
    };
    let member = selected(root, &repository.name, &name).await?;
    let (releases, inventory_error, residual_files) = if repository.kind == "local" {
        match crate::repository_management::Inventory::read(
            &member.state(root),
            member.definition.format(),
            &name,
        )
        .await
        {
            Ok(inventory) => {
                let releases = crate::repository_management::release_files(&inventory)
                    .into_iter()
                    .map(|(version, (files, size))| Release {
                        version,
                        files,
                        size,
                    })
                    .collect();
                let residual = inventory.unassigned.len();
                (Some(releases), None, Some(residual))
            }
            Err(error) => {
                tracing::warn!(%error, repository=%repository.name, "Local inventory unavailable");
                (None, Some("Inventory could not be read. Counts are unknown; cleanup can still be retried.".into()), None)
            }
        }
    } else {
        (None, None, None)
    };
    let mut artifacts = artifacts::read(
        &member.state(root),
        member.definition.format(),
        &name,
        query,
    )
    .await
    .map_err(|status| {
        Box::new((status, "Artifact inventory could not be inspected.").into_response())
    })?;
    let stored_versions = std::mem::take(&mut artifacts.legacy);
    let description = artifacts.description.take();
    let commands = commands(root, &repository, &name);
    Ok(Package {
        repository,
        name,
        source_repository: member.name.clone(),
        commands,
        stored_versions,
        artifacts,
        description,
        releases,
        inventory_error,
        residual_files,
    })
}

async fn api_index(State(root): State<AppState>, Query(query): Query<BrowseQuery>) -> Response {
    match summaries(&root, &query).await {
        Ok(entries) => Json(entries).into_response(),
        Err(response) => *response,
    }
}
async fn api_repository(
    State(root): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Response {
    match listing(&root, &name, &query).await {
        Ok(list) => Json(list).into_response(),
        Err(response) => *response,
    }
}
async fn api_package(
    State(root): State<AppState>,
    Path((repository, name)): Path<(String, String)>,
    Query(query): Query<PackageQuery>,
) -> Response {
    match package_data(&root, &repository, &name, &query).await {
        Ok(package) => Json(package).into_response(),
        Err(response) => *response,
    }
}

fn badge(kind: &str) -> String {
    format!("<span class=\"px-2 py-1 rounded text-xs bg-slate-700 text-slate-300\">{kind}</span>")
}

fn command_block(command: &str) -> String {
    let escaped = components::html_escape(command);
    format!(
        r#"<div class="flex items-center gap-3 bg-slate-900 rounded-lg p-3"><code class="text-sm text-sky-300 flex-1 overflow-x-auto">{escaped}</code><button type="button" class="text-sm text-slate-400 hover:text-white" data-copy="{escaped}" onclick="navigator.clipboard.writeText(this.dataset.copy)" aria-label="Copy command">Copy</button></div>"#
    )
}

fn identity(repository: &Summary, card: bool) -> String {
    let heading = if card { "h2" } else { "h1" };
    let mut body=format!("<div class=\"repository-heading flex items-center gap-3 mb-4\"><{heading} class=\"text-2xl font-bold text-white\">{}</{heading}>{}<span class=\"text-slate-400\">{}</span></div>",components::html_escape(&repository.name),badge(repository.kind),repository.format);
    if repository.kind == "group" {
        let _=write!(body,"<p class=\"text-slate-400 mb-4\">First matching package wins. Member order: {}. Group totals are not added to stored inventory.</p>",components::html_escape(&repository.members.join(" → ")));
    }
    if repository.save_files == Some(false) {
        body.push_str("<p class=\"text-amber-400 mb-4\">New artifact bodies are not persisted. Previously retained files remain readable.</p>");
    }
    body
}

fn page(root: &AppState, headers: &HeaderMap, title: &str, body: &str) -> Response {
    let content = format!(
        r#"<style>
.repository-browser{{overflow-wrap:anywhere}}
.repository-browser .repository-heading{{flex-wrap:wrap}}
.repository-browser .repository-heading>span{{white-space:nowrap;flex-shrink:0}}
.repository-browser .repository-grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,20rem),1fr));gap:1rem}}
.repository-browser .bg-slate-800\/50{{background:rgba(30,41,59,.5)}}
.repository-browser .border-slate-700\/50{{border-color:rgba(51,65,85,.5)}}
.repository-browser .rounded-xl{{border-radius:.75rem}}
.repository-browser .p-5{{padding:1.25rem}}
.repository-browser .text-sky-300,.repository-browser .text-sky-400{{color:#38bdf8}}
.repository-browser .text-amber-400{{color:#fbbf24}}
.repository-browser .hover\:border-sky-500:hover{{border-color:#0ea5e9}}
.repository-browser .divide-y>*+*{{border-top:1px solid #334155}}
.repository-browser code{{min-width:0;white-space:pre;display:block}}
</style><div class="repository-browser">{body}</div>"#
    );
    Html(components::layout_dark(
        title,
        &content,
        Some("repositories"),
        "",
        extract_lang_from_headers(headers),
        root.auth.is_some(),
    ))
    .into_response()
}

async fn index(
    State(root): State<AppState>,
    Query(query): Query<BrowseQuery>,
    headers: HeaderMap,
) -> Response {
    let entries = match summaries(&root, &query).await {
        Ok(entries) => entries,
        Err(response) => return *response,
    };
    let mut body=String::from("<h1 class=\"text-2xl font-bold text-white mb-2\">Repositories</h1><p class=\"text-slate-400 mb-6\">Explicit npm and PyPI endpoints. Browse stored inventory or copy a configured client URL.</p><div class=\"repository-grid\">");
    for repository in entries {
        let _=write!(body,"<a class=\"block bg-slate-800/50 border border-slate-700/50 rounded-xl p-5 hover:border-sky-500\" href=\"/ui/repositories/{}\">{}<p class=\"text-sm text-slate-400\">{}</p></a>",encode_uri_component(&repository.name),identity(&repository,true),repository.stored_artifacts.map(|count|format!("{count} stored artifacts · {}",components::format_size(repository.stored_bytes.unwrap_or(0)))).unwrap_or_else(||"Read-only member view · no separate cache".into()));
    }
    body.push_str("</div>");
    if root.config.repositories.is_empty() {
        body.push_str("<p class=\"text-slate-400 mt-6\">No named repositories are configured. Add [repositories.&lt;name&gt;] in the server configuration. No default npm or PyPI repository is created.</p>");
    }
    page(&root, &headers, "Repositories", &body)
}

async fn repository(
    State(root): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
    headers: HeaderMap,
) -> Response {
    let list = match listing(&root, &name, &query).await {
        Ok(list) => list,
        Err(response) => return *response,
    };
    let mut body=String::from("<a class=\"text-sky-400 text-sm\" href=\"/ui/repositories\">Repositories</a><div class=\"mt-4\">");
    body.push_str(&identity(&list.repository, false));
    body.push_str("</div>");
    for command in commands(&root, &list.repository, "PACKAGE") {
        let _ = write!(
            body,
            "<p class=\"text-sm text-slate-400 mt-4 mb-2\">{}</p>",
            components::html_escape(&command.endpoint)
        );
        body.push_str(&command_block(&command.install));
        if let Some(publish) = command.publish {
            body.push_str(&command_block(&publish));
        }
    }
    let _=write!(body,"<form method=\"get\" class=\"mt-6 mb-4 flex gap-3\"><input type=\"hidden\" name=\"limit\" value=\"{}\"><input name=\"q\" value=\"{}\" aria-label=\"Filter packages\" placeholder=\"Filter known packages\" class=\"bg-slate-800 border border-slate-700 rounded-lg px-3 py-2 text-white\"><button class=\"text-sky-400\">Filter</button></form><p class=\"text-sm text-slate-500 mb-4\">{} known packages. This catalogue never crawls upstreams; a group selects its source when a package is opened.</p><div class=\"bg-slate-800/50 border border-slate-700/50 rounded-xl divide-y divide-slate-700\">",list.limit,components::html_escape(query.q.as_deref().unwrap_or("")),list.total);
    for name in list.packages {
        let _=write!(body,"<a class=\"block p-4 text-sky-400 hover:text-white\" href=\"/ui/repositories/{}/package/{}\">{}</a>",encode_uri_component(&list.repository.name),encode_uri_component(&name),components::html_escape(&name));
    }
    body.push_str("</div><div class=\"mt-4 flex gap-4 text-sky-400\">");
    let q = encode_uri_component(query.q.as_deref().unwrap_or(""));
    if list.page > 1 {
        let _ = write!(
            body,
            "<a href=\"?page={}&amp;limit={}&amp;q={q}\">Previous</a>",
            list.page - 1,
            list.limit
        );
    }
    if list.page < list.total.div_ceil(list.limit) {
        let _ = write!(
            body,
            "<a href=\"?page={}&amp;limit={}&amp;q={q}\">Next</a>",
            list.page.saturating_add(1),
            list.limit
        );
    }
    body.push_str("</div>");
    page(&root, &headers, &list.repository.name, &body)
}

fn delete_form(
    repository: &str,
    name: &str,
    version: Option<&str>,
    label: &str,
    summary: &str,
) -> String {
    let mut action = format!(
        "/api/v1/repositories/{}/packages/{}",
        encode_uri_component(repository),
        encode_uri_component(name)
    );
    if let Some(version) = version {
        let _ = write!(action, "?version={}", encode_uri_component(version));
    }
    let target = version
        .map(|v| format!("{name}@{v}"))
        .unwrap_or_else(|| name.to_owned());
    let confirmation = format!("Permanently delete {target} from {repository}? {summary} All current files for this target will be removed. Re-upload is allowed.");
    format!(
        r#"<form method="post" action="{}" class="delete-local" data-version="{}" data-confirm="{}"><button type="submit" class="text-red-400 hover:text-red-300">{}</button></form>"#,
        components::html_escape(&action),
        components::html_escape(version.unwrap_or("")),
        components::html_escape(&confirmation),
        components::html_escape(label)
    )
}

fn render_local_releases(
    body: &mut String,
    package: &Package,
    retry: Option<&str>,
    can_delete: bool,
) {
    body.push_str("<h2 class=\"text-lg font-semibold text-white mt-8 mb-2\">Releases</h2>");
    let summary = package
        .releases
        .as_ref()
        .map(|releases| {
            format!(
                "Displayed inventory: {} releases, {} distribution files.",
                releases.len(),
                releases.iter().map(|release| release.files).sum::<usize>()
            )
        })
        .unwrap_or_else(|| "Inventory counts are unknown.".into());
    if let Some(error) = &package.inventory_error {
        let _ = write!(
            body,
            "<p role=\"alert\" class=\"text-amber-400\">{}</p>",
            components::html_escape(error)
        );
    }
    if can_delete {
        body.push_str(&delete_form(
            &package.repository.name,
            &package.name,
            None,
            "Delete package",
            &summary,
        ));
        if let Some(version) = retry.filter(|v| crate::repository_management::valid_version(v)) {
            let _ = write!(body, "<p class=\"text-amber-400 mt-4\">A previous deletion may be incomplete. Retry only the selected release.</p>{}", delete_form(&package.repository.name, &package.name, Some(version), "Retry version deletion", "Some files may already be gone."));
        }
    }
    if let Some(count) = package.residual_files.filter(|count| *count > 0) {
        let _ = write!(body, "<p class=\"text-amber-400 mt-4\">{count} files cannot be assigned to a release. Delete the whole package to remove them.</p>");
    }
    body.push_str("<p id=\"delete-error\" role=\"alert\" class=\"text-amber-400 mt-4\" hidden></p><table class=\"w-full text-sm text-left mt-4\"><thead class=\"text-slate-400\"><tr><th class=\"p-3\">Version</th><th class=\"p-3\">Files</th><th class=\"p-3\">Size</th><th class=\"p-3\">Actions</th></tr></thead><tbody class=\"text-slate-300\">");
    if let Some(releases) = &package.releases {
        for release in releases {
            let action = if can_delete {
                delete_form(
                    &package.repository.name,
                    &package.name,
                    Some(&release.version),
                    "Delete version",
                    &format!("Displayed inventory: {} distribution files.", release.files),
                )
            } else {
                String::new()
            };
            let _ = write!(body, "<tr class=\"border-t border-slate-700\"><td class=\"p-3\">{}</td><td class=\"p-3\">{}</td><td class=\"p-3\">{}</td><td class=\"p-3\">{action}</td></tr>", components::html_escape(&release.version), release.files, components::format_size(release.size));
        }
        if releases.is_empty() {
            body.push_str("<tr><td colspan=\"4\" class=\"p-6 text-slate-500\">No releases are stored. Package cleanup can still remove orphan metadata.</td></tr>");
        }
    }
    body.push_str("</tbody></table>");
    if can_delete {
        body.push_str(r#"<script>
document.querySelectorAll('form.delete-local').forEach(function(form) {
    form.addEventListener('submit', async function(event) {
        event.preventDefault();
        if (!window.confirm(form.dataset.confirm)) return;
        const error = document.getElementById('delete-error');
        error.hidden = true;
        const url = new URL(window.location.href);
        if (form.dataset.version) {
            url.searchParams.set('retry_version', form.dataset.version);
            history.replaceState(null, '', url);
        }
        const buttons = document.querySelectorAll('form.delete-local button');
        buttons.forEach(button => button.disabled = true);
        try {
            const response = await fetch(form.action, {method: 'DELETE', headers: {'HX-Request': 'true'}});
            const result = await response.json();
            if (!response.ok) throw new Error(result.message || 'Deletion did not complete. Some files may already be gone; retry or refresh.');
            if (result.package_removed) window.location.assign(document.getElementById('repository-back').href);
            else {
                url.searchParams.delete('retry_version');
                window.location.assign(url.href);
            }
        } catch (failure) {
            error.textContent = 'Deletion was not confirmed. Some files may already be gone or cleanup may still be running. ' + failure.message + ' Refresh this page or retry the same action.';
            error.hidden = false;
            buttons.forEach(button => button.disabled = false);
        }
    });
});
</script>"#);
    }
}

async fn package(
    State(root): State<AppState>,
    Path((repository, name)): Path<(String, String)>,
    Query(query): Query<PackageQuery>,
    role: Option<Extension<crate::auth::AuthenticatedRole>>,
    authority: Option<Extension<crate::auth::NamespaceAuthority>>,
    headers: HeaderMap,
) -> Response {
    let package = match package_data(&root, &repository, &name, &query).await {
        Ok(package) => package,
        Err(response) => return *response,
    };
    let mut body=format!("<a id=\"repository-back\" class=\"text-sky-400 text-sm\" href=\"/ui/repositories/{}\">{}</a><h1 class=\"text-2xl font-bold text-white mt-4 mb-2\">{}</h1><p class=\"text-slate-400 mb-6\">{} repository · selected source: {}</p>",encode_uri_component(&repository),components::html_escape(&repository),components::html_escape(&package.name),package.repository.kind,components::html_escape(&package.source_repository));
    let source_link = format!(
        "<a class=\"text-sky-400\" href=\"/ui/repositories/{}/package/{}\">{}</a>",
        encode_uri_component(&package.source_repository),
        encode_uri_component(&package.name),
        components::html_escape(&package.source_repository)
    );
    body = body.replace(
        &format!(
            "selected source: {}",
            components::html_escape(&package.source_repository)
        ),
        &format!("selected source: {source_link}"),
    );
    let can_delete = package.repository.kind == "local"
        && (!root.config.auth.enabled
            || (role.is_some_and(|Extension(role)| role.0.can_write())
                && authority.is_some_and(|Extension(authority)| {
                    crate::auth::enforce_namespace_scope(&authority, &package.name).is_ok()
                })));
    if let Some(description) = &package.description {
        let _ = write!(
            body,
            "<p class=\"text-slate-300 mb-6\">{}</p>",
            components::html_escape(description)
        );
    }
    for command in &package.commands {
        let _ = write!(
            body,
            "<p class=\"text-sm text-slate-400 mt-4 mb-2\">{}</p>",
            components::html_escape(&command.endpoint)
        );
        body.push_str(&command_block(&command.install));
        if let Some(publish) = &command.publish {
            body.push_str(&command_block(publish));
        }
    }
    if package.repository.kind == "local" {
        render_local_releases(
            &mut body,
            &package,
            query.retry_version.as_deref(),
            can_delete,
        );
        return page(&root, &headers, &package.name, &body);
    }
    body.push_str("<h2 class=\"text-lg font-semibold text-white mt-8 mb-2\">Stored distribution files</h2><p class=\"text-sm text-slate-400 mb-4\">Stored inventory is not a guarantee of offline readiness. Proxy serving still checks current source, integrity and policy.</p><table class=\"w-full text-sm text-left\"><thead class=\"text-slate-400\"><tr><th class=\"p-3\">Version</th><th class=\"p-3\">Size</th><th class=\"p-3\">Updated</th></tr></thead><tbody class=\"text-slate-300\">");
    if package.stored_versions.is_empty() {
        body.push_str("<tr><td colspan=\"3\" class=\"p-6 text-slate-500\">No distribution files are stored for this package in the selected source.</td></tr>");
    }
    for version in package.stored_versions {
        let _=write!(body,"<tr class=\"border-t border-slate-700\"><td class=\"p-3\">{}</td><td class=\"p-3\">{}</td><td class=\"p-3\">{}</td></tr>",components::html_escape(&version.version),components::format_size(version.size),components::html_escape(&version.updated));
    }
    body.push_str("</tbody></table>");
    page(&root, &headers, &package.name, &body)
}
