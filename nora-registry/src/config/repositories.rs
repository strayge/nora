// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! The pre-cutover catalogue: configuration only, independent of legacy routes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

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
    pub paths: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyRepository {
    pub format: RepositoryFormat,
    pub paths: Vec<String>,
    pub upstream: String,
    #[serde(default = "super::default_true")]
    pub save_files: bool,
    #[serde(default = "metadata_ttl")]
    pub metadata_ttl: u64,
    #[serde(default = "connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "super::default_timeout")]
    pub read_timeout: u64,
    #[serde(default = "metadata_timeout")]
    pub metadata_timeout: u64,
    #[serde(default)]
    pub total_timeout: u64,
    #[serde(default = "super::default_true")]
    pub tls_verify: bool,
    #[serde(default)]
    pub tls_ca_cert: Option<String>,
    #[serde(default, skip_serializing)]
    pub auth: Option<ProtectedString>,
    #[serde(default)]
    pub auth_bearer_env: Option<String>,
    #[serde(skip)]
    pub(crate) auth_bearer: Option<ProtectedString>,
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
    for (name, repository) in &config.repositories {
        let label = format!("repositories.{name}");
        let registry = match repository.format() {
            RepositoryFormat::Npm => RegistryType::Npm,
            RepositoryFormat::Pypi => RegistryType::PyPI,
        };
        if !enabled.contains(&registry) {
            errors.push(format!("{label}: format {} is disabled; enable it in [registries].enable or remove this repository", registry.as_str()));
        }
        if !safe_name(name) {
            errors.push(format!(
                "{label}: expected a safe lowercase repository path-segment identity"
            ));
        }
        if repository.paths().is_empty() {
            errors.push(format!("{label}.paths must not be empty"));
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
