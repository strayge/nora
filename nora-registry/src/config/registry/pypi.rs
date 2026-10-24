// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

use crate::secrets::ProtectedString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PypiConfig {
    #[serde(default = "super::super::default_true")]
    pub enabled: bool,
    /// Single upstream — retained for back-compat (`NORA_PYPI_PROXY`, old TOML).
    /// Used only when `proxies` is empty; see [`PypiConfig::upstreams`].
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing)]
    pub proxy_auth: Option<ProtectedString>,
    /// Ordered list of upstreams (#663). The order is the precedence: the first
    /// upstream that lists/serves a file wins, like pip's `--index-url` ahead of
    /// `--extra-index-url`.
    #[serde(default)]
    pub proxies: Vec<PypiProxyEntry>,
    #[serde(default = "super::super::default_timeout")]
    pub proxy_timeout: u64,
}

/// PyPI upstream proxy configuration (mirrors `MavenProxyEntry`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PypiProxyEntry {
    Simple(String),
    Full(PypiProxy),
}

impl<'de> Deserialize<'de> for PypiProxyEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::url_or_table(deserializer, Self::Simple, Self::Full)
    }
}

/// PyPI upstream proxy with optional auth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PypiProxy {
    pub url: String,
    #[serde(default, skip_serializing)]
    pub auth: Option<ProtectedString>,
}

impl PypiProxyEntry {
    pub fn url(&self) -> &str {
        match self {
            PypiProxyEntry::Simple(s) => s,
            PypiProxyEntry::Full(p) => &p.url,
        }
    }
    pub fn auth(&self) -> Option<&str> {
        use crate::secrets::expose_opt;
        match self {
            PypiProxyEntry::Simple(_) => None,
            PypiProxyEntry::Full(p) => expose_opt(&p.auth),
        }
    }
}

impl Default for PypiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            proxy: None,
            proxy_auth: None,
            proxies: Vec::new(),
            proxy_timeout: 30,
        }
    }
}

impl PypiConfig {
    /// Effective, precedence-ordered upstream list.
    ///
    /// `proxies` wins if non-empty (multi-upstream, #663); otherwise the legacy
    /// single `proxy` (+ `proxy_auth`) is used so existing configs keep working;
    /// empty means "no upstream, local-only".
    pub fn upstreams(&self) -> Vec<PypiProxyEntry> {
        if !self.proxies.is_empty() {
            self.proxies.clone()
        } else if let Some(url) = &self.proxy {
            // Fold the legacy single proxy (+ optional auth) into one entry so
            // back-compat users keep their authentication (not silently dropped).
            vec![match &self.proxy_auth {
                Some(auth) => PypiProxyEntry::Full(PypiProxy {
                    url: url.clone(),
                    auth: Some(auth.clone()),
                }),
                None => PypiProxyEntry::Simple(url.clone()),
            }]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_protocol_sources_preserve_auth_without_implicit_defaults() {
        assert!(PypiConfig::default().upstreams().is_empty());
        let mut c = PypiConfig {
            proxy: Some("https://pypi.org/simple/".into()),
            ..PypiConfig::default()
        };
        let u = c.upstreams();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].url(), "https://pypi.org/simple/");
        c.proxy_auth = Some(ProtectedString::new("tok".into()));
        assert_eq!(c.upstreams()[0].auth(), Some("tok"));

        // Explicit proxies win over the legacy single proxy.
        let mut c = PypiConfig::default();
        c.proxies = vec![
            PypiProxyEntry::Simple("https://a/simple".into()),
            PypiProxyEntry::Simple("https://b/simple".into()),
        ];
        let u = c.upstreams();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].url(), "https://a/simple");
        assert_eq!(u[1].url(), "https://b/simple");

        // No proxy at all -> empty (local-only).
        let mut c = PypiConfig::default();
        c.proxy = None;
        assert!(c.upstreams().is_empty());
    }
}
