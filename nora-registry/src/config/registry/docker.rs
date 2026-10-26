// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! Internal protocol settings are constructed from named repository definitions.

use crate::secrets::ProtectedString;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultAction {
    #[default]
    Allow,
    Deny,
}

impl fmt::Display for DefaultAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    pub enabled: bool,
    pub proxy_timeout: u64,
    pub read_timeout: u64,
    pub metadata_ttl: i64,
    pub serve_stale: bool,
    pub default_action: DefaultAction,
    pub upstreams: Vec<DockerUpstream>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerUpstream {
    pub url: String,
    #[serde(default, skip_serializing)]
    pub auth: Option<ProtectedString>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

impl DockerUpstream {
    pub fn resolved_namespace(&self) -> String {
        self.namespace
            .clone()
            .unwrap_or_else(|| extract_docker_namespace(&self.url))
    }
}

pub fn extract_docker_namespace(url: &str) -> String {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("default")
        .split(':')
        .next()
        .unwrap_or("default");
    let host = host.strip_prefix("registry-1.").unwrap_or(host);
    host.strip_prefix("registry.").unwrap_or(host).to_string()
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            proxy_timeout: 300,
            read_timeout: 60,
            metadata_ttl: 0,
            serve_stale: true,
            default_action: DefaultAction::Allow,
            upstreams: Vec::new(),
        }
    }
}
