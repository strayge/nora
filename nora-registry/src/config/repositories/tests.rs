// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;

fn parse(source: &str) -> Config {
    toml::from_str(source).expect("valid catalogue syntax")
}

fn errors(config: &Config) -> Vec<String> {
    config
        .validate_with_config_path(None)
        .1
        .into_iter()
        .filter(|error| error.starts_with("repositories."))
        .collect()
}

fn local(name: &str, format: &str, path: &str) -> String {
    format!("[repositories.{name}]\nformat='{format}'\nkind='local'\npaths=['{path}']\n")
}

fn proxy(extra: &str) -> String {
    format!("[repositories.public]\nformat='npm'\nkind='proxy'\npaths=['/public/']\nupstream='https://registry.example'\n{extra}")
}

fn group_source(format: &str, owners: &str) -> String {
    format!("{}{}[repositories.all]\nformat='{format}'\nkind='group'\npaths=['/all/']\nmembers=['first','second']\nowners={{{owners}}}\n",
        local("first", format, "/first/"), local("second", format, "/second/"))
}

#[test]
fn removed_singletons_cannot_reenable_defaults_but_curation_and_format_selection_remain_valid() {
    let _lock = crate::test_env::env_lock();
    for source in [
        "[npm]",
        "[pypi]",
        "[docker]",
        "[npm]\nproxy='https://registry.example'",
        "[pypi]\nproxies=['https://registry.example']",
    ] {
        let error = toml::from_str::<Config>(source).unwrap_err().to_string();
        assert!(error.contains("[repositories.<name>]"), "{error}");
    }
    let default = Config::default();
    assert!(
        default.repositories.is_empty()
            && default.npm.upstream_urls().is_empty()
            && default.pypi.upstreams().is_empty()
            && default.docker.upstreams.is_empty()
    );
    let config=parse("[registries]\nenable=['npm','pypi']\n[curation.npm]\nquarantine='observe'\n[curation.pypi]\nquarantine='enforce'");
    assert!(config.curation.npm.quarantine.is_some() && config.curation.pypi.quarantine.is_some());
    assert!(errors(&config).is_empty());
}

#[test]
fn a_configured_repository_cannot_silently_enable_a_disabled_format() {
    let _lock = crate::test_env::env_lock();
    for format in ["npm", "pypi"] {
        let config = parse(&format!(
            "[registries]\nenable=['maven']\n{}",
            local("disabled", format, "/disabled/")
        ));
        assert!(errors(&config)
            .iter()
            .any(|error| error.contains("repositories.disabled") && error.contains("disabled")));
    }
}

#[test]
fn plan_example_and_builtins() {
    let _lock = crate::test_env::env_lock();
    let source = r#"
[server]
public_url = "https://packages.example.com"
[repositories.python-local]
format = "pypi"
kind = "local"
paths = ["/repos/python-local/"]
[repositories.python-public]
format = "pypi"
kind = "proxy"
paths = ["/repos/python-public/"]
upstream = "https://pypi.org/simple/"
[repositories.python-private]
format = "pypi"
kind = "proxy"
paths = ["/repos/python-private/"]
upstream = "https://internal.example.com/simple/"
tls_ca_cert = "/etc/nora/private-ca.pem"
save_files = false
[repositories.python-all]
format = "pypi"
kind = "group"
paths = ["/repos/python/", "/simple/"]
members = ["python-private", "python-local", "python-public"]
owners = { "company-*" = "python-private", "local-*" = "python-local" }
[repositories.npm-local]
format = "npm"
kind = "local"
paths = ["/repos/npm-local/"]
[repositories.npm-public]
format = "npm"
kind = "proxy"
paths = ["/repos/npm-public/"]
upstream = "https://registry.npmjs.org"
[repositories.npm-all]
format = "npm"
kind = "group"
paths = ["/repos/npm/", "/npm/"]
members = ["npm-local", "npm-public"]
owners = { "@company/*" = "npm-local" }
"#;
    let config = parse(source);
    assert!(errors(&config).is_empty(), "{:?}", errors(&config));
    let RepositoryConfig::Proxy(public) = &config.repositories["npm-public"] else {
        panic!()
    };
    assert!(public.save_files && public.tls_verify);
    assert_eq!(
        (
            public.metadata_ttl,
            public.connect_timeout,
            public.read_timeout,
            public.metadata_timeout,
            public.total_timeout
        ),
        (300, 5, 30, 10, 0)
    );
    assert!(public.tls_ca_cert.is_none());
    assert!(parse("").repositories.is_empty());
}

#[test]
fn flat_fields_and_applicability_are_always_strict() {
    let _lock = crate::test_env::env_lock();
    for extra in [
        "typo=true",
        "save_files=true",
        "metadata_ttl=300",
        "connect_timeout=5",
        "read_timeout=30",
        "metadata_timeout=10",
        "total_timeout=0",
        "tls_verify=true",
        "tls_ca_cert='ca.pem'",
        "auth='user:password'",
        "auth_bearer_env='TOKEN'",
        "upstream='https://up.example'",
        "members=[]",
        "owners={}",
    ] {
        for kind in ["local", "group"] {
            if kind == "group" && matches!(extra, "members=[]" | "owners={}") {
                continue;
            }
            let source = format!(
                "[repositories.test]\nformat='npm'\nkind='{kind}'\npaths=['/test/']\n{}\n{extra}",
                if kind == "group" {
                    "members=['member']"
                } else {
                    ""
                }
            );
            assert!(super::super::parse_config(&source).is_err(), "{source}");
        }
    }
    for extra in [
        "typo=true",
        "members=[]",
        "owners={}",
        "auth_basic='secret'",
    ] {
        assert!(super::super::parse_config(&proxy(extra)).is_err());
    }
    for source in [
        proxy("").replace(
            "upstream='https://registry.example'",
            "upstream=['https://up.example']",
        ),
        proxy("").replace(
            "upstream='https://registry.example'",
            "upstream={url='https://up.example'}",
        ),
        proxy("").replace("upstream='https://registry.example'", ""),
    ] {
        assert!(toml::from_str::<Config>(&source).is_err());
    }
    assert!(
        super::super::surface_unknown_keys("test", vec!["repository_defaults".into()], false)
            .is_err()
    );
}

#[test]
fn explicit_supported_formats_and_removed_docker_singleton() {
    let _lock = crate::test_env::env_lock();
    for format in ["maven", "NPM", "unknown"] {
        for kind in ["local", "proxy", "group"] {
            let source =
                format!("[repositories.test]\nformat='{format}'\nkind='{kind}'\npaths=['/test/']");
            assert!(toml::from_str::<Config>(&source).is_err());
        }
    }
    let config = parse("[repositories.docker]\nformat='docker'\nkind='local'\n");
    assert!(errors(&config).is_empty());
    assert!(toml::from_str::<Config>("[docker]\nenabled=true\n").is_err());
}

#[test]
fn identities_aliases_and_mount_conflicts() {
    let _lock = crate::test_env::env_lock();
    for name in ["../escape", "Upper", ".hidden", "a.b", "", "a/b"] {
        let source = format!("[repositories.'{name}']\nformat='npm'\nkind='local'\npaths=['/ok/']");
        assert!(!errors(&parse(&source)).is_empty(), "{name}");
    }
    for path in [
        "/",
        "relative/",
        "/no-trailing",
        "/a//b/",
        "/a/../b/",
        "/./",
        "/a%2fb/",
        "/a%5Cb/",
        "/a?b/",
        "/a#b/",
        "/a\\b/",
        "/{route}/",
        "/:capture/",
        "/*wildcard/",
        "/a*b/",
        "/health/",
        "/ready/",
        "/metrics/",
        "/ui/other/",
        "/api/",
        "/api-docs/",
        "/v2/",
        "/maven2/",
    ] {
        assert!(
            !errors(&parse(&local("test", "npm", path))).is_empty(),
            "{path}"
        );
    }
    assert!(errors(&parse(&local("test", "npm", "/simple/"))).is_empty());
    assert!(errors(&parse(&local("test", "pypi", "/npm/"))).is_empty());
    let literal_path = "/@team/a+b/x:y/!$&'(),;=/";
    let source =
        local("test", "npm", "/literal/").replace("'/literal/'", &format!("\"{literal_path}\""));
    assert!(errors(&parse(&source)).is_empty());
    let _router = axum::Router::<()>::new().route(literal_path, axum::routing::get(|| async {}));
    for (a, b) in [("/same/", "/same/"), ("/parent/", "/parent/child/")] {
        assert!(!errors(&parse(&(local("a", "npm", a) + &local("b", "pypi", b)))).is_empty());
    }
    let aliases = local("test", "npm", "/arbitrary/")
        .replace("paths=['/arbitrary/']", "paths=['/arbitrary/','/npm/']");
    assert!(errors(&parse(&aliases)).is_empty());
    assert!(errors(&parse(&local("test", "pypi", "/simple/"))).is_empty());
    assert!(!errors(&parse(&aliases.replace("'/npm/'", "'/arbitrary/'"))).is_empty());
    assert!(!errors(&parse(&aliases.replace("['/arbitrary/','/npm/']", "[]"))).is_empty());
    let mut config = parse(&local("test", "npm", "/maven2/"));
    config.maven.enabled = false;
    assert!(errors(&config).is_empty());
    let mut config = parse(&local("test", "npm", "/pwsh/"));
    assert!(errors(&config).is_empty());
    config.nuget.enabled = true;
    assert!(!errors(&config).is_empty());
}

#[test]
fn group_reference_validation() {
    let _lock = crate::test_env::env_lock();
    let base = group_source("pypi", "");
    for source in [
        base.replace("['first','second']", "[]"),
        base.replace("['first','second']", "['missing']"),
        base.replace("['first','second']", "['first','first']"),
        base.replace("['first','second']", "['all']"),
        base.replacen("format='pypi'", "format='npm'", 1),
    ] {
        assert!(!errors(&parse(&source)).is_empty(), "{source}");
    }
    assert!(toml::from_str::<Config>(&group_source("pypi", "'pkg'='missing'")).is_err());
}

#[test]
fn compiled_pypi_rules_normalize_once_and_keep_matcher_kinds() {
    let _lock = crate::test_env::env_lock();
    let config = parse(&group_source(
        "pypi",
        "'Company__.*'='first','COMPANY_Pkg'='second','company*'='second'",
    ));
    let RepositoryConfig::Group(group) = &config.repositories["all"] else {
        panic!()
    };
    let rules = &group.compiled_owners;
    assert_eq!(rules.owner("company-pkg"), Some("second"));
    assert_eq!(rules.owner("company-other"), Some("first"));
    assert_eq!(rules.owner("companywide"), Some("second"));
    assert_eq!(rules.owner("unowned"), None);
    for owners in [
        "'Company_Pkg'='first','company-pkg'='first'",
        "'Company__*'='first','company-*'='second'",
    ] {
        assert!(toml::from_str::<Config>(&group_source("pypi", owners)).is_err());
    }
    let config = parse(&group_source("pypi", "'pkg'='first','pkg*'='second'"));
    let RepositoryConfig::Group(group) = &config.repositories["all"] else {
        panic!()
    };
    assert_eq!(group.compiled_owners.owner("pkg"), Some("first"));
    assert_eq!(group.compiled_owners.owner("pkg-extra"), Some("second"));
}

#[test]
fn npm_legacy_read_names_preserve_case_and_punctuation() {
    let _lock = crate::test_env::env_lock();
    let config = parse(&group_source(
        "npm",
        "'Legacy.Name'='first','legacy.name'='second','@Company/*'='first','Old~Pkg!'='second'",
    ));
    let RepositoryConfig::Group(group) = &config.repositories["all"] else {
        panic!()
    };
    for (name, owner) in [
        ("Legacy.Name", "first"),
        ("legacy.name", "second"),
        ("@Company/Some_Pkg", "first"),
        ("Old~Pkg!", "second"),
    ] {
        assert_eq!(group.compiled_owners.owner(name), Some(owner));
    }
    assert_eq!(group.compiled_owners.owner("@company/Some_Pkg"), None);
    for pattern in [
        "a*b",
        "a**",
        "a?b",
        "a/b",
        "@scope/a/b",
        "@/pkg",
        "../pkg",
        "@scope",
        "bad%2fname",
    ] {
        assert!(
            toml::from_str::<Config>(&group_source("npm", &format!("'{pattern}'='first'")))
                .is_err(),
            "{pattern}"
        );
    }
}

#[test]
fn timeouts_integer_units_and_tls_conflicts() {
    let _lock = crate::test_env::env_lock();
    for extra in [
        "metadata_ttl=-1",
        "total_timeout=-1",
        "read_timeout='30s'",
        "metadata_ttl=1.5",
    ] {
        assert!(toml::from_str::<Config>(&proxy(extra)).is_err());
    }
    for extra in [
        "connect_timeout=0",
        "read_timeout=0",
        "metadata_timeout=0",
        "tls_verify=false\ntls_ca_cert='ca.pem'",
    ] {
        assert!(!errors(&parse(&proxy(extra))).is_empty());
    }
    assert!(errors(&parse(&proxy("metadata_ttl=0\ntotal_timeout=0"))).is_empty());
    for extra in ["tls_verify=false", "tls_ca_cert='ca.pem'"] {
        let source = proxy(extra).replace("https://registry.example", "http://registry.example");
        assert!(!errors(&parse(&source)).is_empty());
    }
    let mut config = parse(&proxy("tls_verify=false"));
    config.tls.ca_cert = Some("global.pem".into());
    assert!(errors(&config).is_empty());
    assert!(config
        .validate_with_config_path(None)
        .0
        .iter()
        .any(|w| w.contains("verification is disabled")));
}

#[test]
fn credentials_resolve_and_never_enter_diagnostics() {
    let _lock = crate::test_env::env_lock();
    let mut config = parse(&proxy("auth_bearer_env='PATH'"));
    config.apply_env_overrides().unwrap();
    let RepositoryConfig::Proxy(public) = config.repositories.get_mut("public").unwrap() else {
        panic!()
    };
    assert!(public.auth_bearer.is_some());
    public.auth_bearer = Some(ProtectedString::from("a\nsecret"));
    assert!(!errors(&config).is_empty());
    for extra in [
        "auth_bearer_env='NORA_REPOSITORIES_TEST_UNSET'",
        "auth='user:password'\nauth_bearer_env='PATH'",
    ] {
        assert!(!errors(&parse(&proxy(extra))).is_empty());
    }
    let basic = parse(&proxy("auth='user:password'"));
    let RepositoryConfig::Proxy(public) = &basic.repositories["public"] else {
        panic!()
    };
    assert_eq!(
        public
            .auth()
            .unwrap()
            .header_value()
            .unwrap()
            .to_str()
            .unwrap(),
        super::super::basic_auth_header("user:password")
    );
    assert!(!format!("{basic:?}").contains("password"));
    assert!(!toml::to_string(&basic).unwrap().contains("password"));
    let source = proxy("auth_bearer_env='PATH'").replace("format='npm'", "format='pypi'");
    assert!(errors(&parse(&source))
        .iter()
        .any(|e| e.contains("npm-only")));
    let sentinel = "S01-UPSTREAM-CREDENTIAL-MARKER";
    let config = parse(&proxy("").replace(
        "https://registry.example",
        &format!("ftp://user:{sentinel}@host"),
    ));
    let diagnostics = format!("{:?}{:?}", config, errors(&config));
    assert!(!diagnostics.contains(sentinel));
    let source = proxy(&format!("typo=true\nauth='user:{sentinel}'"));
    let error = super::super::parse_config(&source).unwrap_err().to_string();
    assert!(error.contains("unknown field"));
    assert!(!error.contains(sentinel));
}

#[test]
fn named_upstream_leak_patterns_use_the_host_without_url_credentials() {
    for format in ["npm", "pypi"] {
        let source = proxy("")
            .replace("format='npm'", &format!("format='{format}'"))
            .replace(
                "https://registry.example",
                "https://user:password@private.example:443/simple/",
            );
        assert!(parse(&source)
            .upstream_hostnames()
            .contains(&(format.into(), "private.example".into())));
    }
}

#[test]
fn invalid_catalogue_in_default_config_is_fatal_without_strict_mode() {
    let _lock = crate::test_env::env_lock();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), proxy("save_fiels=false")).unwrap();
    let result = Config::load_from(None, file.path().to_str().unwrap(), false);
    assert!(matches!(
        result,
        Err(super::super::ConfigLoadError::InvalidToml { .. })
    ));
}
