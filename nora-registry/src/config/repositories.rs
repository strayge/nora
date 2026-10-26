// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! The pre-cutover catalogue: configuration only, independent of legacy routes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod docker_tests;
#[cfg(test)]
mod tests;

use super::{Config, UpstreamAuth};
use crate::registry_type::RegistryType;
use crate::secrets::{expose_opt, ProtectedString};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryFormat {
    Npm,
    Pypi,
    Docker,
}

impl RepositoryFormat {
    pub fn registry_type(self) -> RegistryType {
        match self {
            Self::Npm => RegistryType::Npm,
            Self::Pypi => RegistryType::PyPI,
            Self::Docker => RegistryType::Docker,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RepositoryConfig {
    Local(LocalRepository),
    Proxy(ProxyRepository),
    Group(GroupRepository),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRepository {
    pub format: RepositoryFormat,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "ProxyDefinition", into = "ProxyDefinition")]
pub struct ProxyRepository {
    pub format: RepositoryFormat,
    pub paths: Vec<String>,
    pub port: Option<u16>,
    pub public_url: Option<String>,
    pub upstream: String,
    pub save_files: bool,
    pub metadata_ttl: u64,
    pub connect_timeout: u64,
    pub read_timeout: u64,
    pub metadata_timeout: u64,
    pub total_timeout: u64,
    pub proxy_timeout: Option<u64>,
    pub tls_verify: bool,
    pub tls_ca_cert: Option<String>,
    pub auth: Option<ProtectedString>,
    pub auth_bearer_env: Option<String>,
    pub(crate) auth_bearer: Option<ProtectedString>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProxyDefinition {
    format: RepositoryFormat,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_url: Option<String>,
    upstream: String,
    #[serde(default, skip_serializing)]
    auth: Option<ProtectedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    save_files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_ttl: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_verify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_ca_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_bearer_env: Option<String>,
}

impl TryFrom<ProxyDefinition> for ProxyRepository {
    type Error = String;

    fn try_from(raw: ProxyDefinition) -> Result<Self, Self::Error> {
        let docker = raw.format == RepositoryFormat::Docker;
        if docker {
            for (field, present) in [
                ("save_files", raw.save_files.is_some()),
                ("connect_timeout", raw.connect_timeout.is_some()),
                ("metadata_timeout", raw.metadata_timeout.is_some()),
                ("total_timeout", raw.total_timeout.is_some()),
                ("tls_verify", raw.tls_verify.is_some()),
                ("tls_ca_cert", raw.tls_ca_cert.is_some()),
                ("auth_bearer_env", raw.auth_bearer_env.is_some()),
            ] {
                if present {
                    return Err(format!("{field} is not supported for Docker repositories"));
                }
            }
        } else if raw.proxy_timeout.is_some() {
            return Err("proxy_timeout is Docker-only".into());
        }
        Ok(Self {
            format: raw.format,
            paths: raw.paths,
            port: raw.port,
            public_url: raw.public_url,
            upstream: raw.upstream,
            auth: raw.auth,
            save_files: raw.save_files.unwrap_or(true),
            metadata_ttl: raw
                .metadata_ttl
                .unwrap_or_else(|| if docker { 0 } else { metadata_ttl() }),
            connect_timeout: raw.connect_timeout.unwrap_or_else(connect_timeout),
            read_timeout: raw.read_timeout.unwrap_or_else(|| {
                if docker {
                    60
                } else {
                    super::default_timeout()
                }
            }),
            metadata_timeout: raw.metadata_timeout.unwrap_or_else(metadata_timeout),
            total_timeout: raw.total_timeout.unwrap_or(0),
            proxy_timeout: docker.then(|| raw.proxy_timeout.unwrap_or(300)),
            tls_verify: raw.tls_verify.unwrap_or(true),
            tls_ca_cert: raw.tls_ca_cert,
            auth_bearer_env: raw.auth_bearer_env,
            auth_bearer: None,
        })
    }
}

impl From<ProxyRepository> for ProxyDefinition {
    fn from(proxy: ProxyRepository) -> Self {
        let docker = proxy.format == RepositoryFormat::Docker;
        Self {
            format: proxy.format,
            paths: proxy.paths,
            port: proxy.port,
            public_url: proxy.public_url,
            upstream: proxy.upstream,
            auth: proxy.auth,
            save_files: (!docker).then_some(proxy.save_files),
            metadata_ttl: Some(proxy.metadata_ttl),
            connect_timeout: (!docker).then_some(proxy.connect_timeout),
            read_timeout: Some(proxy.read_timeout),
            metadata_timeout: (!docker).then_some(proxy.metadata_timeout),
            total_timeout: (!docker).then_some(proxy.total_timeout),
            proxy_timeout: proxy.proxy_timeout,
            tls_verify: (!docker).then_some(proxy.tls_verify),
            tls_ca_cert: proxy.tls_ca_cert,
            auth_bearer_env: proxy.auth_bearer_env,
        }
    }
}

impl std::fmt::Debug for ProxyRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // URL userinfo and query parameters can contain credentials too.
        f.debug_struct("ProxyRepository")
            .field("format", &self.format)
            .field("paths", &self.paths)
            .field("upstream", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

fn metadata_ttl() -> u64 {
    300
}

fn connect_timeout() -> u64 {
    5
}

fn metadata_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "GroupDefinition")]
pub struct GroupRepository {
    pub format: RepositoryFormat,
    pub paths: Vec<String>,
    pub members: Vec<String>,
    pub owners: BTreeMap<String, String>,
    #[serde(skip)]
    pub compiled_owners: CompiledOwners,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupDefinition {
    format: RepositoryFormat,
    paths: Vec<String>,
    members: Vec<String>,
    #[serde(default)]
    owners: BTreeMap<String, String>,
}

/// Selection consumes canonical coordinates without changing a format's name identity.
#[derive(Debug, Clone, Default)]
pub struct CompiledOwners {
    exact: BTreeMap<String, String>,
    prefixes: Vec<(String, String)>,
}

impl CompiledOwners {
    /// Coordinates must already be canonicalized at the format's request boundary.
    pub fn owner(&self, coordinate: &str) -> Option<&str> {
        self.exact.get(coordinate).map(String::as_str).or_else(|| {
            self.prefixes
                .iter()
                .find(|(prefix, _)| coordinate.starts_with(prefix))
                .map(|(_, member)| member.as_str())
        })
    }
}

impl TryFrom<GroupDefinition> for GroupRepository {
    type Error = String;

    fn try_from(raw: GroupDefinition) -> Result<Self, Self::Error> {
        let mut compiled = CompiledOwners::default();
        let mut seen = BTreeSet::new();
        for (pattern, member) in &raw.owners {
            if !raw.members.contains(member) {
                return Err("owners: every target must be a direct group member".into());
            }
            let prefix = pattern.ends_with('*');
            let literal = if prefix {
                &pattern[..pattern.len() - 1]
            } else {
                pattern.as_str()
            };
            let literal = canonical_pattern(raw.format, literal, prefix)?;
            if !seen.insert((prefix, literal.clone())) {
                return Err(
                    "owners: duplicate canonical matcher (including normalization collisions)"
                        .into(),
                );
            }
            if prefix {
                compiled.prefixes.push((literal, member.clone()));
            } else {
                compiled.exact.insert(literal, member.clone());
            }
        }
        compiled
            .prefixes
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        Ok(Self {
            format: raw.format,
            paths: raw.paths,
            members: raw.members,
            owners: raw.owners,
            compiled_owners: compiled,
        })
    }
}

/// Validate literal read coordinates, not npm's stricter new-publication rules.
fn canonical_pattern(
    format: RepositoryFormat,
    literal: &str,
    prefix: bool,
) -> Result<String, String> {
    if prefix && literal.is_empty() {
        return Ok(String::new());
    }
    let valid = match format {
        RepositoryFormat::Docker => return Err("Docker groups are not supported".into()),
        RepositoryFormat::Pypi => {
            !literal.is_empty()
                && literal.starts_with(|c: char| c.is_ascii_alphanumeric())
                && (prefix || literal.ends_with(|c: char| c.is_ascii_alphanumeric()))
                && literal
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        }
        RepositoryFormat::Npm => {
            let segment = |s: &str| {
                !s.is_empty()
                    && !s.starts_with(['.', '_'])
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-._~!()'".contains(&c))
            };
            if let Some(scoped) = literal.strip_prefix('@') {
                match scoped.split_once('/') {
                    Some((scope, package)) => {
                        segment(scope) && (segment(package) || (prefix && package.is_empty()))
                    }
                    None => prefix && segment(scoped),
                }
            } else {
                segment(literal)
            }
        }
    };
    if !valid {
        return Err(
            "owners: expected a valid package name or literal prefix followed by one trailing '*'"
                .into(),
        );
    }
    if format == RepositoryFormat::Npm {
        return Ok(literal.to_string());
    }
    let mut normalized = String::new();
    for c in literal.bytes() {
        if b"-_.".contains(&c) {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.push((c as char).to_ascii_lowercase());
        }
    }
    Ok(normalized)
}

impl RepositoryConfig {
    pub fn port(&self, server: &super::ServerConfig) -> u16 {
        match self {
            Self::Local(r) => r.port,
            Self::Proxy(r) => r.port,
            Self::Group(_) => None,
        }
        .unwrap_or(server.port)
    }

    pub fn public_url(&self) -> Option<&str> {
        match self {
            Self::Local(r) => r.public_url.as_deref(),
            Self::Proxy(r) => r.public_url.as_deref(),
            Self::Group(_) => None,
        }
    }

    pub fn docker_public_url(&self, server: &super::ServerConfig) -> String {
        if let Some(url) = self.public_url() {
            return url.trim_end_matches('/').to_owned();
        }
        let mut url =
            reqwest::Url::parse(&server.public_base_url()).expect("validated server public URL");
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        if self.port(server) != server.port {
            url.set_port(Some(self.port(server)))
                .expect("HTTP URL accepts a port");
        }
        url.as_str().trim_end_matches('/').to_owned()
    }

    pub fn format(&self) -> RepositoryFormat {
        match self {
            Self::Local(r) => r.format,
            Self::Proxy(r) => r.format,
            Self::Group(r) => r.format,
        }
    }

    pub fn paths(&self) -> &[String] {
        match self {
            Self::Local(r) => &r.paths,
            Self::Proxy(r) => &r.paths,
            Self::Group(r) => &r.paths,
        }
    }
}

impl ProxyRepository {
    /// Match existing npm credential resolution without adding repository env-key syntax.
    pub(super) fn resolve_credentials(&mut self) {
        self.auth_bearer = self.auth_bearer_env.as_ref().and_then(|var| {
            std::env::var(var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(ProtectedString::new)
        });
    }

    #[allow(dead_code)] // Transport integration follows in a later slice.
    pub fn auth(&self) -> Option<UpstreamAuth<'_>> {
        match &self.auth_bearer {
            Some(token) => Some(UpstreamAuth::Bearer(token.expose())),
            None => expose_opt(&self.auth).map(UpstreamAuth::Basic),
        }
    }

    fn problems(&self, label: &str, warnings: &mut Vec<String>, errors: &mut Vec<String>) {
        let url = reqwest::Url::parse(&self.upstream)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
        if url.is_none() {
            errors.push(format!(
                "{label}.upstream must be an http(s) URL with a host"
            ));
        }
        for (key, value) in [
            ("connect_timeout", self.connect_timeout),
            ("read_timeout", self.read_timeout),
            ("metadata_timeout", self.metadata_timeout),
        ] {
            if value == 0 {
                errors.push(format!("{label}.{key} must be positive integer seconds"));
            }
        }
        if !self.tls_verify && self.tls_ca_cert.is_some() {
            errors.push(format!(
                "{label}: tls_ca_cert conflicts with tls_verify=false"
            ));
        }
        if url.as_ref().is_some_and(|url| url.scheme() == "http")
            && (!self.tls_verify || self.tls_ca_cert.is_some())
        {
            errors.push(format!("{label}: TLS exceptions require an HTTPS upstream"));
        }
        if !self.tls_verify {
            warnings.push(format!(
                "{label}: upstream certificate and hostname verification is disabled"
            ));
        }
        if self.format == RepositoryFormat::Docker {
            if url.as_ref().is_some_and(|url| {
                !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
            }) {
                errors.push(format!(
                    "{label}.upstream must not contain credentials, query or fragment"
                ));
            }
            if self.metadata_ttl > i64::MAX as u64 {
                errors.push(format!("{label}.metadata_ttl exceeds supported seconds"));
            }
            if self.proxy_timeout == Some(0) {
                errors.push(format!(
                    "{label}.proxy_timeout must be positive integer seconds"
                ));
            }
        }
        if self.auth.is_some() && self.auth_bearer_env.is_some() {
            errors.push(format!(
                "{label}: set either auth (Basic) or auth_bearer_env (Bearer), not both"
            ));
        }
        if self.auth_bearer_env.is_some() {
            if self.format != RepositoryFormat::Npm {
                errors.push(format!("{label}: auth_bearer_env is npm-only"));
            }
            match &self.auth_bearer {
                None => errors.push(format!("{label}: auth_bearer_env is unset or empty")),
                Some(token) if UpstreamAuth::Bearer(token.expose()).header_value().is_err() => {
                    errors.push(format!(
                        "{label}: Bearer token has characters an HTTP header cannot carry"
                    ));
                }
                Some(_) => {}
            }
        }
    }
}

/// A restricted identity alphabet avoids case-folding and filesystem-normalization aliases.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_".contains(&c))
}

/// Restrict mount literals to unescaped URL path segments, never Axum route syntax.
fn canonical_path(path: &str) -> bool {
    path.len() > 1
        && path.starts_with('/')
        && path.ends_with('/')
        && path[1..path.len() - 1].split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                // Axum's default route checks reserve a leading colon for old capture syntax.
                && !segment.starts_with(':')
                && segment
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._~!$&'()+,;=:@".contains(&c))
        })
}

fn overlaps(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Explicit aliases must not overlap another active endpoint or silently enable a disabled format.
pub(super) fn problems(config: &Config) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut mounts: Vec<&str> = Vec::new();
    let mut reserved = vec![
        "/health/",
        "/ready/",
        "/metrics/",
        "/ui/",
        "/api/",
        "/api-docs/",
    ];
    let enabled = config.enabled_registries_quiet();
    for registry in enabled.iter().copied() {
        if !matches!(registry, RegistryType::Npm | RegistryType::PyPI) {
            reserved.push(registry.mount_point());
        }
        if registry == RegistryType::Nuget {
            reserved.extend(["/chocolatey/", "/pwsh/"]);
        }
    }
    let mut docker_ports = BTreeMap::new();
    for (name, repository) in &config.repositories {
        let label = format!("repositories.{name}");
        let registry = repository.format().registry_type();
        if !enabled.contains(&registry) {
            errors.push(format!("{label}: format {} is disabled; enable it in [registries].enable or remove this repository", registry.as_str()));
        }
        if !safe_name(name) {
            errors.push(format!(
                "{label}: expected a safe lowercase repository path-segment identity"
            ));
        }
        if repository.format() == RepositoryFormat::Docker {
            if matches!(repository, RepositoryConfig::Group(_)) {
                errors.push(format!("{label}: Docker groups are not supported"));
            }
            if !repository.paths().is_empty() {
                errors.push(format!(
                    "{label}.paths: Docker repositories use ports, not paths"
                ));
            }
            let port = repository.port(&config.server);
            if port == 0 {
                errors.push(format!("{label}.port must be nonzero"));
            }
            if let Some(previous) = docker_ports.insert(port, name) {
                errors.push(format!(
                    "{label}: Docker port {port} conflicts with repositories.{previous}"
                ));
            }
            if let Some(raw) = repository.public_url() {
                let valid = reqwest::Url::parse(raw).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && matches!(url.path(), "" | "/")
                        && url.query().is_none()
                        && url.fragment().is_none()
                });
                if !valid {
                    errors.push(format!("{label}.public_url must be an HTTP(S) origin without credentials, path, query or fragment"));
                }
            }
        } else {
            if repository.paths().is_empty() {
                errors.push(format!("{label}.paths must not be empty"));
            }
            let explicit_port = match repository {
                RepositoryConfig::Local(r) => r.port.is_some(),
                RepositoryConfig::Proxy(r) => r.port.is_some(),
                RepositoryConfig::Group(_) => false,
            };
            if explicit_port || repository.public_url().is_some() {
                errors.push(format!("{label}: port and public_url are Docker-only"));
            }
        }
        for path in repository.paths() {
            if !canonical_path(path) {
                errors.push(format!(
                    "{label}.paths: expected canonical leading/trailing-slash paths"
                ));
                continue;
            }
            if mounts.iter().any(|other| overlaps(path, other))
                || reserved.iter().any(|other| overlaps(path, other))
            {
                errors.push(format!(
                    "{label}.paths: conflicting or reserved mount {path}"
                ));
            }
            mounts.push(path);
        }
        match repository {
            RepositoryConfig::Proxy(proxy) => proxy.problems(&label, &mut warnings, &mut errors),
            RepositoryConfig::Group(group) => {
                let mut seen = BTreeSet::new();
                if group.members.is_empty() {
                    errors.push(format!("{label}.members must not be empty"));
                }
                for member in &group.members {
                    if !seen.insert(member) {
                        errors.push(format!("{label}.members: duplicate member"));
                    }
                    match config.repositories.get(member) {
                        None => errors.push(format!("{label}.members: unknown repository")),
                        Some(RepositoryConfig::Group(_)) => {
                            errors.push(format!("{label}.members: nested groups are not supported"))
                        }
                        Some(repository) if repository.format() != group.format => {
                            errors.push(format!("{label}.members: mixed formats are not supported"))
                        }
                        Some(_) => {}
                    }
                }
                if group
                    .owners
                    .values()
                    .any(|owner| !group.members.contains(owner))
                {
                    errors.push(format!("{label}.owners: targets must be direct members"));
                }
            }
            RepositoryConfig::Local(_) => {}
        }
    }
    (warnings, errors)
}
