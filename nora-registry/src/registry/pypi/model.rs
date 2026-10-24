// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Public URLs are rendered separately from the source's distribution metadata.

use crate::ui::components::html_escape;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub(super) type Hashes = BTreeMap<String, String>;

pub(super) fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Project {
    pub name: String,
    pub files: Vec<Distribution>,
    pub versions: Option<Vec<String>>,
    pub status: ProjectStatus,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Distribution {
    pub filename: String,
    pub url: String,
    pub hashes: Hashes,
    pub requires_python: Option<String>,
    pub yanked: Option<Yanked>,
    pub core_metadata: Option<CoreMetadata>,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum Yanked {
    Flag(bool),
    Reason(String),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum CoreMetadata {
    Flag(bool),
    Hashes(Hashes),
}

impl CoreMetadata {
    pub fn available(&self) -> bool {
        match self {
            Self::Flag(value) => *value,
            Self::Hashes(hashes) => !hashes.is_empty(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Status {
    #[default]
    Active,
    Archived,
    Deprecated,
    Quarantined,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deprecated => "deprecated",
            Self::Quarantined => "quarantined",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ProjectStatus {
    pub status: Status,
    pub reason: Option<String>,
}

fn api_version(version: &str) -> Result<(), &'static str> {
    let (major, minor) = version
        .split_once('.')
        .ok_or("invalid project API version")?;
    if major != "1" || minor.is_empty() || !minor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("unsupported project API version");
    }
    Ok(())
}

fn hashes(value: &Value) -> Result<Hashes, &'static str> {
    let object = value.as_object().ok_or("invalid hash dictionary")?;
    let mut hashes = Hashes::new();
    for (name, value) in object {
        let digest = value.as_str().ok_or("invalid hash value")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || digest.is_empty()
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid hash dictionary");
        }
        if hashes
            .insert(name.to_ascii_lowercase(), digest.to_ascii_lowercase())
            .is_some()
        {
            return Err("duplicate hash algorithm");
        }
    }
    Ok(hashes)
}

fn fragment_hashes(url: &Url) -> Result<Hashes, &'static str> {
    let mut hashes = Hashes::new();
    if let Some(fragment) = url.fragment() {
        for part in fragment.split('&') {
            if let Some((algorithm, digest)) = part.split_once('=') {
                if algorithm.is_empty()
                    || !algorithm
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    || digest.is_empty()
                    || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    continue;
                }
                if hashes
                    .insert(algorithm.to_ascii_lowercase(), digest.to_ascii_lowercase())
                    .is_some()
                {
                    return Err("duplicate fragment hash");
                }
            }
        }
    }
    Ok(hashes)
}

fn string(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err("invalid string metadata"),
    }
}

fn timestamp(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    let Some(value) = string(value)? else {
        return Ok(None);
    };
    let date = chrono::DateTime::parse_from_rfc3339(&value).map_err(|_| "invalid upload time")?;
    Ok(Some(
        date.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    ))
}

fn location(base: &Url, href: &str) -> Result<Url, &'static str> {
    let url = base.join(href).map_err(|_| "invalid distribution URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("unsupported distribution URL scheme");
    }
    Ok(url)
}

fn version_key(version: &str) -> String {
    crate::curation::pep440_key(version).unwrap_or_else(|| version.to_ascii_lowercase())
}

fn accepted(name: &str, filename: &str) -> bool {
    super::is_valid_pypi_filename(filename)
        && nora_registry::pypi_files::parse_pypi_version(name, filename).is_some()
}

pub(super) fn parse_json(bytes: &[u8], name: &str, base: &Url) -> Result<Project, &'static str> {
    let document: Value = serde_json::from_slice(bytes).map_err(|_| "invalid project JSON")?;
    let version = document
        .get("meta")
        .and_then(|meta| meta.get("api-version"))
        .and_then(Value::as_str)
        .ok_or("missing project API version")?;
    api_version(version)?;
    let upstream_name = document
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing project name")?;
    if nora_registry::pypi_files::pep503_normalize(upstream_name) != name {
        return Err("project name mismatch");
    }
    let entries = document
        .get("files")
        .and_then(Value::as_array)
        .ok_or("missing project files")?;
    let versions = match document.get("versions") {
        None => None,
        Some(value) => Some(
            value
                .as_array()
                .ok_or("invalid project versions")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("invalid project version")
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    let status = match document.get("project-status") {
        None => ProjectStatus::default(),
        Some(value) => {
            let value = value.as_object().ok_or("invalid project status")?;
            ProjectStatus {
                status: value
                    .get("status")
                    .map(|value| {
                        serde_json::from_value(value.clone()).map_err(|_| "invalid project status")
                    })
                    .transpose()?
                    .unwrap_or_default(),
                reason: string(value.get("reason"))?,
            }
        }
    };
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let filename = entry
            .get("filename")
            .and_then(Value::as_str)
            .ok_or("missing distribution filename")?;
        if !accepted(name, filename) {
            continue;
        }
        if !seen.insert(filename) {
            return Err("duplicate distribution filename");
        }
        let href = entry
            .get("url")
            .and_then(Value::as_str)
            .ok_or("missing distribution URL")?;
        let mut url = location(base, href)?;
        let mut file_hashes = hashes(entry.get("hashes").ok_or("missing distribution hashes")?)?;
        for (algorithm, digest) in fragment_hashes(&url)? {
            if file_hashes
                .get(&algorithm)
                .is_some_and(|existing| existing != &digest)
            {
                return Err("conflicting distribution hashes");
            }
            file_hashes.insert(algorithm, digest);
        }
        url.set_fragment(None);
        let yanked = match entry.get("yanked") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(Yanked::Flag(*value)),
            Some(Value::String(reason)) if reason.is_empty() => Some(Yanked::Flag(true)),
            Some(Value::String(reason)) => Some(Yanked::Reason(reason.clone())),
            _ => return Err("invalid yanked metadata"),
        };
        let core_metadata = match entry
            .get("core-metadata")
            .or_else(|| entry.get("dist-info-metadata"))
        {
            None => None,
            Some(Value::Bool(value)) => Some(CoreMetadata::Flag(*value)),
            Some(value) => Some(CoreMetadata::Hashes(hashes(value)?)),
        };
        let size = match entry.get("size") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or("invalid distribution size")?),
        };
        files.push(Distribution {
            filename: filename.into(),
            url: url.to_string(),
            hashes: file_hashes,
            requires_python: string(entry.get("requires-python"))?,
            yanked,
            core_metadata,
            size,
            upload_time: timestamp(entry.get("upload-time"))?,
        });
    }
    if let Some(versions) = &versions {
        let keys: BTreeSet<_> = versions
            .iter()
            .map(|version| version_key(version))
            .collect();
        if files
            .iter()
            .filter_map(|file| nora_registry::pypi_files::parse_pypi_version(name, &file.filename))
            .any(|version| !keys.contains(&version_key(&version)))
        {
            return Err("incomplete authoritative versions");
        }
    }
    Ok(Project {
        name: name.into(),
        files,
        versions,
        status,
    })
}

pub(super) fn parse_html(bytes: &[u8], name: &str, base: &Url) -> Result<Project, &'static str> {
    static ANCHORS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("a[href]").expect("static anchor selector"));
    static META: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("head meta[name][content]").expect("static metadata selector")
    });
    static BASE: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("head base[href]").expect("static base selector"));
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid project HTML encoding")?;
    let document = Html::parse_document(text);
    let document_base = match document
        .select(&BASE)
        .next()
        .and_then(|base| base.value().attr("href"))
    {
        Some(href) => location(base, href)?,
        None => base.clone(),
    };
    let mut status = ProjectStatus::default();
    let mut explicit_status = false;
    for meta in document.select(&META) {
        let element = meta.value();
        match element.attr("name") {
            Some("pypi:repository-version") => {
                api_version(element.attr("content").unwrap_or_default())?
            }
            Some("pypi:project-status") => {
                if explicit_status {
                    return Err("duplicate project status");
                }
                explicit_status = true;
                status.status = serde_json::from_value(Value::String(
                    element.attr("content").unwrap_or_default().into(),
                ))
                .map_err(|_| "invalid project status")?;
            }
            Some("pypi:project-status-reason") => {
                status.reason = element.attr("content").map(str::to_owned)
            }
            _ => {}
        }
    }
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for anchor in document.select(&ANCHORS) {
        let element = anchor.value();
        let Ok(mut url) = location(&document_base, element.attr("href").unwrap_or_default()) else {
            continue;
        };
        let Some(last) = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
        else {
            continue;
        };
        let Ok(filename) = percent_encoding::percent_decode_str(last).decode_utf8() else {
            continue;
        };
        let filename = filename.into_owned();
        if !accepted(name, &filename) {
            continue;
        }
        if !seen.insert(filename.clone()) {
            return Err("duplicate distribution filename");
        }
        let file_hashes = fragment_hashes(&url)?;
        url.set_fragment(None);
        let core_metadata = match element
            .attr("data-core-metadata")
            .or_else(|| element.attr("data-dist-info-metadata"))
        {
            None => None,
            Some("false") => Some(CoreMetadata::Flag(false)),
            Some("" | "true") => Some(CoreMetadata::Flag(true)),
            Some(value) => {
                let (algorithm, digest) =
                    value.split_once('=').ok_or("invalid Core Metadata hash")?;
                Some(CoreMetadata::Hashes(hashes(&json!({algorithm: digest}))?))
            }
        };
        files.push(Distribution {
            filename,
            url: url.to_string(),
            hashes: file_hashes,
            requires_python: element.attr("data-requires-python").map(str::to_owned),
            yanked: element.attr("data-yanked").map(|reason| {
                if reason.is_empty() {
                    Yanked::Flag(true)
                } else {
                    Yanked::Reason(reason.into())
                }
            }),
            core_metadata,
            size: None,
            upload_time: None,
        });
    }
    Ok(Project {
        name: name.into(),
        files,
        versions: None,
        status,
    })
}

pub(super) fn file_url(base: &str, name: &str, filename: &str, binding: Option<&str>) -> String {
    const SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'+');
    // Installers append `.metadata` to the whole URL, so bindings must be path segments.
    let base = match binding {
        Some(binding) => format!(
            "{}/-/source/{}",
            base.trim_end_matches('/'),
            percent_encoding::utf8_percent_encode(binding, percent_encoding::NON_ALPHANUMERIC)
        ),
        None => base.trim_end_matches('/').into(),
    };
    format!(
        "{base}/{}/{}",
        percent_encoding::utf8_percent_encode(name, SAFE),
        percent_encoding::utf8_percent_encode(filename, SAFE)
    )
}

pub(super) fn render_json(
    project: &Project,
    files: &[Distribution],
    base: &str,
    full: bool,
    sidecars: bool,
    binding: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    let rich = files.iter().all(|file| file.size.is_some());
    let files = if project.status.status == Status::Quarantined {
        &[]
    } else {
        files
    };
    let mut output = json!({"meta": {"api-version": if rich { "1.4" } else { "1.0" }}, "name": project.name, "files": []});
    let mut entries = Vec::new();
    for file in files {
        let mut entry = json!({"filename":file.filename, "url":file_url(base, &project.name, &file.filename, binding), "hashes":file.hashes});
        if let Some(value) = &file.requires_python {
            entry["requires-python"] = json!(value);
        }
        if let Some(value) = &file.yanked {
            entry["yanked"] = serde_json::to_value(value)?;
        }
        if sidecars {
            if let Some(value) = file
                .core_metadata
                .as_ref()
                .filter(|value| value.available())
            {
                entry["core-metadata"] = serde_json::to_value(value)?;
            }
        }
        if rich {
            entry["size"] = json!(file.size);
            if let Some(value) = &file.upload_time {
                entry["upload-time"] = json!(value);
            }
        }
        entries.push(entry);
    }
    output["files"] = json!(entries);
    if rich {
        let inferred = || {
            files
                .iter()
                .filter_map(|file| {
                    let version = nora_registry::pypi_files::parse_pypi_version(
                        &project.name,
                        &file.filename,
                    )?;
                    if let Some(authoritative) = &project.versions {
                        if let Some(original) = authoritative
                            .iter()
                            .find(|candidate| *candidate == &version)
                            .or_else(|| {
                                authoritative.iter().find(|candidate| {
                                    version_key(candidate) == version_key(&version)
                                })
                            })
                        {
                            return Some(original.clone());
                        }
                    }
                    Some(crate::curation::pep440_normalized(&version).unwrap_or(version))
                })
                .collect::<BTreeSet<_>>()
        };
        let versions: BTreeSet<_> = if full {
            project
                .versions
                .as_ref()
                .map(|versions| versions.iter().cloned().collect())
                .unwrap_or_else(inferred)
        } else {
            inferred()
        };
        output["versions"] = json!(versions);
        output["project-status"] = json!({"status":project.status.status.as_str()});
        if let Some(reason) = &project.status.reason {
            output["project-status"]["reason"] = json!(reason);
        }
    }
    serde_json::to_vec(&output)
}

pub(super) fn render_html(
    project: &Project,
    files: &[Distribution],
    base: &str,
    sidecars: bool,
    binding: Option<&str>,
) -> String {
    use std::fmt::Write;
    let name = html_escape(&project.name);
    let mut html = format!("<!DOCTYPE html>\n<html><head><title>Links for {name}</title><meta name=\"pypi:repository-version\" content=\"1.4\"><meta name=\"pypi:project-status\" content=\"{}\">", project.status.status.as_str());
    if let Some(reason) = &project.status.reason {
        let _ = write!(
            html,
            "<meta name=\"pypi:project-status-reason\" content=\"{}\">",
            html_escape(reason)
        );
    }
    let _ = writeln!(html, "</head><body><h1>Links for {name}</h1>");
    if project.status.status != Status::Quarantined {
        for file in files {
            let mut url = file_url(base, &project.name, &file.filename, binding);
            if let Some((algorithm, digest)) = file
                .hashes
                .get_key_value("sha256")
                .or_else(|| file.hashes.iter().next())
            {
                let _ = write!(url, "#{algorithm}={digest}");
            }
            let _ = write!(html, "<a href=\"{}\"", html_escape(&url));
            if let Some(value) = &file.requires_python {
                let _ = write!(html, " data-requires-python=\"{}\"", html_escape(value));
            }
            match &file.yanked {
                Some(Yanked::Flag(true)) => html.push_str(" data-yanked=\"\""),
                Some(Yanked::Reason(reason)) => {
                    let _ = write!(html, " data-yanked=\"{}\"", html_escape(reason));
                }
                _ => {}
            }
            if sidecars {
                if let Some(value) = file
                    .core_metadata
                    .as_ref()
                    .filter(|value| value.available())
                {
                    let value = match value {
                        CoreMetadata::Hashes(hashes) => hashes
                            .iter()
                            .next()
                            .map(|(algorithm, digest)| format!("{algorithm}={digest}"))
                            .unwrap_or_default(),
                        CoreMetadata::Flag(_) => "true".into(),
                    };
                    let _ = write!(html, " data-core-metadata=\"{}\"", html_escape(&value));
                }
            }
            let _ = writeln!(html, ">{}</a>", html_escape(&file.filename));
        }
    }
    html.push_str("</body></html>");
    html
}

#[cfg(test)]
mod tests;
