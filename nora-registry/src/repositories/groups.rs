// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Selection is package-wide: neither an error nor a missing selected file reopens it.

use super::{Repository, RepositoryCatalogue};
use crate::config::repositories::GroupRepository;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::Arc;

pub(crate) struct Group {
    pub name: String,
    pub definition: GroupRepository,
}
impl Group {
    pub fn registry_type(&self) -> crate::registry_type::RegistryType {
        match self.definition.format {
            crate::config::repositories::RepositoryFormat::Npm => {
                crate::registry_type::RegistryType::Npm
            }
            crate::config::repositories::RepositoryFormat::Pypi => {
                crate::registry_type::RegistryType::PyPI
            }
        }
    }
}

pub(crate) enum Lookup<T, E> {
    Present(T),
    Absent,
    Failure(E),
}

pub(crate) struct Selection<T> {
    pub repository: Arc<Repository>,
    pub binding: String,
    pub package: T,
}

#[derive(Debug)]
pub(crate) enum ResolveError<E> {
    Failure(E),
    Binding,
    MissingMember,
}

pub(crate) fn binding(repository: &Repository) -> String {
    let mut hash = Sha256::new();
    hash.update(repository.name.as_bytes());
    hash.update([0]);
    hash.update(repository.source_id.as_bytes());
    hash.update([0]);
    hash.update(match repository.definition.format() {
        crate::config::repositories::RepositoryFormat::Npm => b"npm".as_slice(),
        crate::config::repositories::RepositoryFormat::Pypi => b"pypi".as_slice(),
    });
    hex::encode(hash.finalize())
}

pub(crate) fn bound_prefix(prefix: &str, binding: &str) -> String {
    format!("{}-/source/{binding}/", prefix)
}

/// Binding tokens are locators, never permission to choose a different member.
pub(crate) async fn resolve<T, E, F, Fut>(
    catalogue: &RepositoryCatalogue,
    group: &Group,
    coordinate: &str,
    expected: Option<&str>,
    mut lookup: F,
) -> Result<Option<Selection<T>>, ResolveError<E>>
where
    F: FnMut(Arc<Repository>) -> Fut,
    Fut: Future<Output = Lookup<T, E>>,
{
    if let Some(expected) = expected {
        if !group.definition.members.iter().any(|name| {
            catalogue
                .get(name)
                .is_some_and(|repository| binding(repository) == expected)
        }) {
            return Err(ResolveError::Binding);
        }
    }
    let owner = group.definition.compiled_owners.owner(coordinate);
    for name in group
        .definition
        .members
        .iter()
        .filter(|name| owner.is_none_or(|owner| name.as_str() == owner))
    {
        let repository = catalogue
            .get(name)
            .ok_or(ResolveError::MissingMember)?
            .clone();
        match lookup(repository.clone()).await {
            Lookup::Present(package) => {
                let binding = binding(&repository);
                if expected.is_some_and(|expected| expected != binding) {
                    return Err(ResolveError::Binding);
                }
                return Ok(Some(Selection {
                    repository,
                    binding,
                    package,
                }));
            }
            Lookup::Absent => {}
            Lookup::Failure(error) => return Err(ResolveError::Failure(error)),
        }
    }
    Ok(None)
}

pub(crate) fn failure(
    error: ResolveError<Box<axum::response::Response>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match error {
        ResolveError::Failure(response) => *response,
        ResolveError::Binding => axum::http::StatusCode::NOT_FOUND.into_response(),
        ResolveError::MissingMember => {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::BTreeMap;

    async fn catalogue() -> RepositoryCatalogue {
        let config: Config = toml::from_str(
            r#"
[repositories.a]
format="pypi"
kind="local"
paths=["/a/"]
[repositories.b]
format="pypi"
kind="local"
paths=["/b/"]
[repositories.c]
format="pypi"
kind="local"
paths=["/c/"]
[repositories.group]
format="pypi"
kind="group"
paths=["/group/"]
members=["a","b","c"]
owners={"owned*"="b","owned-exact"="c","owned-long*"="c"}
"#,
        )
        .unwrap();
        let storage = crate::storage::Storage::from_backend(Arc::new(
            crate::storage::ObjectStorage::in_memory(),
        ));
        RepositoryCatalogue::open(&config, &storage).await.unwrap()
    }

    #[tokio::test]
    async fn one_policy_matrix_covers_precedence_absence_failure_and_empty_presence() {
        let catalogue = catalogue().await;
        let group = catalogue.groups().next().unwrap();
        for (name, outcomes, expected, calls) in [
            ("unowned", [0, 1, 1], Some("b"), vec!["a", "b"]),
            ("unowned", [1, 1, 1], Some("a"), vec!["a"]),
            ("unowned", [2, 1, 1], None, vec!["a"]),
            ("owned-pkg", [1, 0, 1], None, vec!["b"]),
            ("owned-pkg", [1, 2, 1], None, vec!["b"]),
            ("owned-exact", [1, 1, 1], Some("c"), vec!["c"]),
            ("owned-long-name", [1, 1, 1], Some("c"), vec!["c"]),
        ] {
            let outcomes =
                BTreeMap::from([("a", outcomes[0]), ("b", outcomes[1]), ("c", outcomes[2])]);
            let mut seen = Vec::new();
            let result = resolve(&catalogue, group, name, None, |repository| {
                let outcome = outcomes[repository.name.as_str()];
                seen.push(repository.name.clone());
                std::future::ready(match outcome {
                    0 => Lookup::Absent,
                    1 => Lookup::Present(Vec::<u8>::new()),
                    _ => Lookup::Failure("unavailable"),
                })
            })
            .await;
            assert_eq!(seen, calls);
            assert_eq!(
                result.is_err(),
                calls.last().is_some_and(|name| outcomes[name] == 2)
            );
            match result {
                Ok(result) => assert_eq!(
                    result
                        .as_ref()
                        .map(|selection| selection.repository.name.as_str()),
                    expected
                ),
                Err(ResolveError::Failure(_)) => assert!(expected.is_none()),
                _ => panic!("unexpected selection result"),
            }
        }
    }

    #[tokio::test]
    async fn a_locator_must_still_match_current_membership_priority_ownership_and_source() {
        let catalogue = catalogue().await;
        let group = catalogue.groups().next().unwrap();
        let a = binding(catalogue.get("a").unwrap());
        let b = binding(catalogue.get("b").unwrap());
        for (name, token, valid) in [
            ("unowned", a.as_str(), true),
            ("unowned", b.as_str(), false),
            ("owned-x", a.as_str(), false),
            ("owned-x", b.as_str(), true),
            ("unowned", "old-source", false),
        ] {
            let result = resolve(&catalogue, group, name, Some(token), |_| {
                std::future::ready(Lookup::<_, ()>::Present(()))
            })
            .await;
            assert_eq!(result.is_ok(), valid);
            if !valid {
                assert!(matches!(result, Err(ResolveError::Binding)));
            }
        }
    }
}
