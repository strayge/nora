// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Protocol fixtures use the production member constructor and scoped router without
//! descriptor I/O. Identity, restart and maintenance tests must use real catalogue open.

pub use super::*;
use crate::config::repositories::{
    LocalRepository, ProxyRepository, RepositoryConfig, RepositoryFormat,
};

fn context(
    users: &[(&str, &str)],
    auth: bool,
    anonymous: bool,
    configure: impl FnOnce(&mut Config),
    storage: Option<Storage>,
    egress: Option<&str>,
) -> TestContext {
    let mut ctx = super::build_context_full(
        auth,
        users,
        anonymous,
        |config| {
            configure(config);
            assert!(
                config.docker.upstreams.len() <= 1,
                "named Docker proxies have one upstream"
            );
            let definition = match config.docker.upstreams.first() {
                None => RepositoryConfig::Local(LocalRepository {
                    format: RepositoryFormat::Docker,
                    paths: vec![],
                    port: None,
                    public_url: None,
                }),
                Some(upstream) => RepositoryConfig::Proxy(ProxyRepository {
                    format: RepositoryFormat::Docker,
                    paths: vec![],
                    port: None,
                    public_url: None,
                    upstream: upstream.url.clone(),
                    auth: upstream.auth.clone(),
                    metadata_ttl: config.docker.metadata_ttl.max(0) as u64,
                    read_timeout: config.docker.read_timeout,
                    proxy_timeout: Some(config.docker.proxy_timeout),
                    save_files: true,
                    connect_timeout: 5,
                    metadata_timeout: 10,
                    total_timeout: 0,
                    tls_verify: true,
                    tls_ca_cert: None,
                    auth_bearer_env: None,
                    auth_bearer: None,
                }),
            };
            config.repositories.insert("docker".into(), definition);
        },
        storage,
        egress,
    );
    ctx.state.repositories = Arc::new(crate::repositories::RepositoryCatalogue::protocol_fixture(
        &ctx.state.config,
        &ctx.state.storage,
    ));
    ctx.app = super::router_for_state(&ctx.state);
    let view = ctx
        .state
        .repositories
        .get("docker")
        .unwrap()
        .state(&ctx.state);
    ctx.root = Some(ctx.state.clone());
    ctx.state = view;
    ctx
}

pub fn create_test_context() -> TestContext {
    context(&[], false, false, |_| {}, None, None)
}

pub fn create_test_context_with_config(configure: impl FnOnce(&mut Config)) -> TestContext {
    context(&[], false, false, configure, None, None)
}

pub fn create_test_context_with_auth(users: &[(&str, &str)]) -> TestContext {
    context(users, true, false, |_| {}, None, None)
}

pub fn create_test_context_with_anonymous_read(users: &[(&str, &str)]) -> TestContext {
    context(users, true, true, |_| {}, None, None)
}

pub fn create_test_context_with_docker_anon_pull(users: &[(&str, &str)]) -> TestContext {
    context(
        users,
        true,
        false,
        |c| c.auth.docker_anon_pull = true,
        None,
        None,
    )
}

pub fn create_test_context_with_storage(storage: Storage) -> TestContext {
    context(&[], false, false, |_| {}, Some(storage), None)
}

pub fn create_test_context_with_storage_and_config(
    storage: Storage,
    configure: impl FnOnce(&mut Config),
) -> TestContext {
    context(&[], false, false, configure, Some(storage), None)
}
