// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;

fn config(tables: &str) -> Config {
    toml::from_str(tables).unwrap()
}

#[test]
fn effective_ports_include_the_main_port_default() {
    let _lock = crate::test_env::env_lock();
    let mut cfg = config(
        r#"
[repositories.a]
format = "docker"
kind = "local"
[repositories.b]
format = "docker"
kind = "local"
port = 4000
"#,
    );
    assert!(problems(&cfg).1.iter().any(|e| e.contains("4000")
        && e.contains("repositories.a")
        && e.contains("repositories.b")));
    cfg.server.port = 10433;
    assert!(problems(&cfg).1.is_empty());
    let RepositoryConfig::Local(b) = cfg.repositories.get_mut("b").unwrap() else {
        panic!()
    };
    b.port = Some(10433);
    assert!(!problems(&cfg).1.is_empty());
}

#[test]
fn docker_policy_defaults_and_redacted_serialization_round_trip() {
    let _lock = crate::test_env::env_lock();
    let cfg = config(
        r#"
[repositories.cache]
format = "docker"
kind = "proxy"
port = 19443
upstream = "https://registry.example.com/base"
auth = "user:secret"
"#,
    );
    let RepositoryConfig::Proxy(proxy) = &cfg.repositories["cache"] else {
        panic!()
    };
    assert_eq!(
        (proxy.metadata_ttl, proxy.read_timeout, proxy.proxy_timeout),
        (0, 60, Some(300))
    );
    let text = toml::to_string(&cfg).unwrap();
    assert!(!text.contains("user:secret"));
    let document: toml::Value = toml::from_str(&text).unwrap();
    assert!(document["repositories"]["cache"]
        .get("save_files")
        .is_none());
    assert!(document["repositories"]["cache"].get("auth").is_none());
    let round_trip: Config = toml::from_str(&text).unwrap();
    assert!(problems(&round_trip).1.is_empty());
    assert!(round_trip.docker.upstreams.is_empty());
}

#[test]
fn unsupported_docker_fields_are_rejected_even_at_their_usual_defaults() {
    let _lock = crate::test_env::env_lock();
    let base = "[repositories.cache]\nformat='docker'\nkind='proxy'\nupstream='https://registry.example.com'\n";
    for field in [
        "save_files=true",
        "connect_timeout=5",
        "metadata_timeout=10",
        "total_timeout=0",
        "tls_verify=true",
        "tls_ca_cert='ca.pem'",
        "auth_bearer_env='TOKEN'",
    ] {
        let error = toml::from_str::<Config>(&format!("{base}{field}\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(field.split('=').next().unwrap()), "{error}");
    }
}

#[test]
fn docker_endpoints_are_origin_only_and_other_formats_remain_path_based() {
    let _lock = crate::test_env::env_lock();
    for endpoint in [
        "https://user:pass@registry.example.com",
        "https://registry.example.com/base",
        "https://registry.example.com?q=x",
        "https://registry.example.com/#x",
    ] {
        let cfg = config(&format!(
            "[repositories.a]\nformat='docker'\nkind='local'\npublic_url='{endpoint}'\n"
        ));
        assert!(problems(&cfg).1.iter().any(|e| e.contains("public_url")));
    }
    let mut cfg = config("[repositories.a]\nformat='docker'\nkind='local'\n");
    cfg.server.public_url = Some("https://[::1]:9443/ui-base/".into());
    assert_eq!(
        cfg.repositories["a"].docker_public_url(&cfg.server),
        "https://[::1]:9443"
    );
    let RepositoryConfig::Local(local) = cfg.repositories.get_mut("a").unwrap() else {
        panic!()
    };
    local.port = Some(19443);
    assert_eq!(
        cfg.repositories["a"].docker_public_url(&cfg.server),
        "https://[::1]:19443"
    );
    let cfg = config("[repositories.a]\nformat='npm'\nkind='local'\npaths=['/a/']\nport=19443\n");
    assert!(problems(&cfg).1.iter().any(|e| e.contains("Docker-only")));
}

#[test]
fn docker_rejects_group_aliases_zero_ports_and_removed_singleton() {
    let _lock = crate::test_env::env_lock();
    for definition in [
        "kind='local'\npaths=['/docker/']",
        "kind='local'\nport=0",
        "kind='group'\npaths=[]\nmembers=[]",
    ] {
        let cfg = config(&format!(
            "[repositories.a]\nformat='docker'\n{definition}\n"
        ));
        assert!(!problems(&cfg).1.is_empty());
    }
    assert!(toml::from_str::<Config>("[docker]\n").is_err());
}
