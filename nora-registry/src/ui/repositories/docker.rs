// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Docker image coordinates do not use the package-file or local-delete model.

use super::*;

#[derive(Serialize)]
pub(super) struct Data {
    repository: Summary,
    name: String,
    login: String,
    commands: Vec<Commands>,
    #[serde(flatten)]
    detail: super::super::api::DockerDetail,
}

pub(super) fn is_docker(root: &AppState, repository: &str) -> bool {
    root.config
        .repositories
        .get(repository)
        .is_some_and(|definition| definition.format() == RepositoryFormat::Docker)
}

pub(super) async fn data(
    root: &AppState,
    repository: &str,
    name: &str,
) -> Result<Data, StatusCode> {
    crate::validation::validate_docker_name(name).map_err(|_| StatusCode::BAD_REQUEST)?;
    let summary = summary(root, repository)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let member = root
        .repositories
        .get(repository)
        .ok_or(StatusCode::NOT_FOUND)?;
    let view = member.state(root);
    let detail = super::super::api::get_docker_detail(&view, name)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if detail.tags.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let endpoint = summary.public_url.as_ref().expect("Docker public endpoint");
    let authority = endpoint
        .split_once("://")
        .expect("validated Docker origin")
        .1;
    let login = format!("docker login {}", shell_arg(authority));
    let commands = commands(root, &summary, name);
    Ok(Data {
        repository: summary,
        name: name.into(),
        login,
        commands,
        detail,
    })
}

pub(super) async fn render(
    root: &AppState,
    repository: &str,
    name: &str,
    headers: &HeaderMap,
) -> Response {
    let data = match data(root, repository, name).await {
        Ok(data) => data,
        Err(status) => return status.into_response(),
    };
    let mut body = format!("<a class=\"text-sky-400\" href=\"/ui/repositories/{}\">{}</a><h1 class=\"text-2xl font-bold mt-4 text-white\">{}</h1>", encode_uri_component(repository), components::html_escape(repository), components::html_escape(name));
    body.push_str(&identity(&data.repository, true));
    body.push_str(&command_block(&data.login));
    body.push_str("<p class=\"text-sm text-slate-400 my-4\">Stored tags only. Browsing never refreshes upstream metadata.</p><div class=\"divide-y divide-slate-700\">");
    for tag in data.detail.tags {
        let _ = write!(body, "<section class=\"py-4\"><h2 class=\"text-lg font-semibold text-white\">{}</h2><p class=\"text-sm text-slate-400\">{} · {} pulls · {}/{}</p>", components::html_escape(&tag.name), components::format_size(tag.size), tag.downloads, components::html_escape(&tag.os), components::html_escape(&tag.arch));
        body.push_str(&command_block(&tag.pull_command));
        if data.repository.kind == "local" {
            let endpoint = data
                .repository
                .public_url
                .as_ref()
                .expect("Docker endpoint");
            let authority = endpoint.split_once("://").expect("validated origin").1;
            let target = shell_arg(&format!("{authority}/{name}:{}", tag.name));
            body.push_str(&command_block(&format!(
                "docker tag '<local-image>:<tag>' {target}"
            )));
            body.push_str(&command_block(&format!("docker push {target}")));
        }
        body.push_str("</section>");
    }
    body.push_str("</div>");
    page(root, headers, &format!("{repository} · {name}"), &body)
}
