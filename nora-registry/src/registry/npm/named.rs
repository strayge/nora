// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Named metadata stays independent of the request's public alias on disk.

use super::*;

#[derive(Clone)]
pub(super) struct Mount(pub(super) Arc<str>);

pub(super) fn bind(state: &mut AppState, mount: Mount) {
    state.repository_mount = Some(mount.0);
}

pub(super) fn base(state: &AppState) -> String {
    format!(
        "{}{}",
        nora_base_url(state).trim_end_matches('/'),
        state
            .repository_mount
            .as_deref()
            .unwrap_or("/npm/")
            .trim_end_matches('/')
    )
}

pub(super) fn for_storage(state: &AppState, data: &[u8], upstream: &str, path: &str) -> Vec<u8> {
    if state.repository.is_some() {
        data.to_vec()
    } else {
        rewrite_for_nora(state, data, upstream, path)
    }
}

fn segment(value: &str) -> String {
    const SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'@');
    percent_encoding::utf8_percent_encode(value, SAFE).to_string()
}

fn route(name: &str, version: &str) -> Result<String, StatusCode> {
    if !crate::validation::is_one_segment(version) {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let package = name.split('/').map(segment).collect::<Vec<_>>().join("/");
    let short = name.rsplit('/').next().unwrap_or(name);
    Ok(format!(
        "{package}/-/{}",
        segment(&format!("{short}-{version}.tgz"))
    ))
}

pub(super) fn render(state: &AppState, data: Bytes, name: &str) -> Result<Bytes, StatusCode> {
    let Some(repository) = &state.repository else {
        return Ok(data);
    };
    let mut json: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| StatusCode::BAD_GATEWAY)?;
    let base = base(state);
    let render_version =
        |document: &mut serde_json::Value, version: &str| -> Result<(), StatusCode> {
            if let Some(dist) = document
                .get_mut("dist")
                .and_then(|value| value.as_object_mut())
            {
                if dist.get("tarball").is_some_and(|value| value.is_string()) {
                    dist.insert(
                        "tarball".into(),
                        serde_json::Value::String(format!("{base}/{}", route(name, version)?)),
                    );
                }
            }
            Ok(())
        };
    if let Some(versions) = json
        .get_mut("versions")
        .and_then(|value| value.as_object_mut())
    {
        for (version, document) in versions {
            render_version(document, version)?;
        }
    }
    if json
        .get("dist")
        .and_then(|dist| dist.get("tarball"))
        .is_some()
    {
        let version = json
            .get("version")
            .and_then(|value| value.as_str())
            .ok_or(StatusCode::BAD_GATEWAY)?
            .to_owned();
        render_version(&mut json, &version)?;
    }
    let bytes = serde_json::to_vec(&json).map_err(|_| StatusCode::BAD_GATEWAY)?;
    if let crate::config::repositories::RepositoryConfig::Proxy(proxy) = &repository.definition {
        Ok(Bytes::from(replace_upstream_bytes(
            &bytes,
            proxy.upstream.trim_end_matches('/'),
            &base,
        )))
    } else {
        Ok(Bytes::from(bytes))
    }
}

pub(super) fn response(state: &AppState, headers: &HeaderMap, data: Bytes, name: &str) -> Response {
    match render(state, data, name) {
        Ok(data) => packument_response(headers, data),
        Err(status) => status.into_response(),
    }
}

/// Locked downloads resolve the same exact version as a packument-generated URL.
pub(super) async fn artifact_url(
    state: &AppState,
    name: &str,
    version: &str,
) -> Result<reqwest::Url, StatusCode> {
    let upstream = state.config.npm.route(name).ok_or(StatusCode::NOT_FOUND)?;
    let key = upstream_packument_key(name);
    let cached = if cached_copy_trusted(state, name, &key, name, None).await {
        match state.storage.get_verified(&key).await {
            Ok(nora_registry::verified::GateOutcome::Verified(blob)) => {
                Some(nora_registry::verified::verified_body(blob))
            }
            _ => None,
        }
    } else {
        None
    };
    let data = match cached {
        Some(data) => data,
        None => {
            let url = format!("{}/{}", upstream.url.trim_end_matches('/'), name);
            let data = proxy_fetch(
                state,
                &url,
                Duration::from_secs(state.config.npm.proxy_timeout),
                upstream.auth,
                &state.circuit_breaker,
                BreakerScope::upstream(RegistryType::Npm, upstream.url),
            )
            .await
            .map_err(|error| match error {
                ProxyError::NotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_GATEWAY,
            })?;
            record_first_seen(state, &key, &data, None).await;
            let _ = put_with_origin(&state.storage, &key, &data, &origin_of(upstream.url)).await;
            Bytes::from(data)
        }
    };
    let json: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| StatusCode::BAD_GATEWAY)?;
    let tarball = json
        .get("versions")
        .and_then(|versions| versions.get(version))
        .and_then(|document| document.get("dist"))
        .and_then(|dist| dist.get("tarball"))
        .and_then(|url| url.as_str())
        .ok_or(StatusCode::NOT_FOUND)?;
    let configured = reqwest::Url::parse(upstream.url).map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut base = configured.clone();
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    let target = base.join(tarball).map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !matches!(target.scheme(), "http" | "https")
        || (configured.scheme() == "https" && target.scheme() != "https")
    {
        return Err(StatusCode::BAD_GATEWAY);
    }
    Ok(target)
}
