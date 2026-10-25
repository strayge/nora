// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Recorded metadata describes observations, never current serving eligibility.

use serde::Serialize;
use std::collections::BTreeSet;

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize)]
pub(crate) struct Checksum {
    pub algorithm: String,
    pub value: String,
    pub representation: &'static str,
    pub source: String,
    pub saved_at: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Distribution {
    pub version: String,
    pub filename: Option<String>,
    pub size: Option<u64>,
    pub checksums: Vec<Checksum>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceState {
    Available,
    Missing,
    Absent,
    Unavailable,
    NotApplicable,
}

#[derive(Serialize)]
pub(crate) struct Source {
    pub source: String,
    pub state: SourceState,
    pub saved_at: Option<u64>,
    pub expired: Option<bool>,
}

#[derive(Default)]
pub(crate) struct Metadata {
    pub versions: BTreeSet<String>,
    pub files: Vec<Distribution>,
    pub sources: Vec<Source>,
    pub warnings: Vec<&'static str>,
    pub description: Option<String>,
    pub authoritative_versions: Option<Vec<String>>,
    pub exhaustive: bool,
}

impl Metadata {
    pub fn complete(&self) -> bool {
        self.warnings.is_empty()
            && self
                .sources
                .iter()
                .all(|source| source.state != SourceState::Unavailable)
    }

    pub fn warn(&mut self, warning: &'static str) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}
