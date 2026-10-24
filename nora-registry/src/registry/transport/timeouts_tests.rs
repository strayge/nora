// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use crate::config::Config;
use crate::registry_type::RegistryType;
use crate::test_helpers::{create_test_context_with_config, with_named_repositories, TestContext};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn context(url: &str, configure: impl FnOnce(&mut Config)) -> TestContext {
    with_named_repositories(create_test_context_with_config(|config| {
        config.repositories = toml::from_str::<Config>(&format!(
            r#"
[repositories.first]
format="npm"
kind="proxy"
paths=["/first/"]
upstream={url:?}
connect_timeout=1
read_timeout=5
metadata_timeout=1
[repositories.second]
format="npm"
kind="proxy"
paths=["/second/"]
upstream={url:?}
metadata_timeout=3
"#
        ))
        .unwrap()
        .repositories;
        config.circuit_breaker.enabled = true;
        config.circuit_breaker.failure_threshold = 1;
        config.circuit_breaker.reset_timeout = 1;
        configure(config);
    }))
    .await
}

fn first_policy(config: &mut Config) -> &mut ProxyRepository {
    let crate::config::repositories::RepositoryConfig::Proxy(policy) =
        config.repositories.get_mut("first").unwrap()
    else {
        panic!()
    };
    policy
}

async fn fetch(state: &AppState, url: &str) -> Result<Vec<u8>, ProxyError> {
    proxy_fetch(
        state,
        url,
        Duration::from_secs(30),
        None,
        &state.circuit_breaker,
        BreakerScope::upstream(RegistryType::Npm, url),
    )
    .await
}

async fn head(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut buffer = [0; 1024];
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count != 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 16384);
    }
}

async fn progressing(chunks: usize) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        head(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for _ in 0..chunks {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if stream.write_all(b"4\r\ndata\r\n").await.is_err() {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n").await;
    });
    (uri, task)
}

#[tokio::test]
async fn profiles_and_breakers_are_isolated_even_for_the_same_upstream() {
    let _lock = crate::test_env::env_lock();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("metadata")
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&server)
        .await;
    let context = context(&server.uri(), |_| {}).await;
    let first = context.state.repositories.get("first").unwrap();
    let second = context.state.repositories.get("second").unwrap();
    let first_state = first.state(&context.state);
    let second_state = second.state(&context.state);
    let url = format!("{}/pkg", server.uri());
    let (short, longer) = tokio::join!(fetch(&first_state, &url), fetch(&second_state, &url));
    assert!(short.is_err());
    assert_eq!(longer.unwrap(), b"metadata");
    assert_ne!(first.breaker_key(), second.breaker_key());
    assert_eq!(
        context
            .state
            .circuit_breaker
            .health_snapshot(&first.breaker_key())
            .unwrap()
            .status,
        "open"
    );
    assert_eq!(
        context
            .state
            .circuit_breaker
            .health_snapshot(&second.breaker_key())
            .unwrap()
            .status,
        "closed"
    );
    let gauges = crate::circuit_breaker::initial_gauge_keys(&context.state.config);
    assert!(gauges.contains(&first.breaker_key()));
    assert!(gauges.contains(&second.breaker_key()));
    let mut changed = context.state.config.as_ref().clone();
    first_policy(&mut changed).upstream = "https://different.example/".into();
    let changed_key = crate::circuit_breaker::repository_key(
        RegistryType::Npm,
        "first",
        &crate::repositories::source_id(&changed.repositories["first"]).unwrap(),
    );
    assert_ne!(changed_key, first.breaker_key());
    assert!(!changed_key.contains("different.example"));
}

#[tokio::test]
async fn retry_backoff_cannot_multiply_the_metadata_budget() {
    let _lock = crate::test_env::env_lock();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(600)))
        .mount(&server)
        .await;
    let context = context(&server.uri(), |_| {}).await;
    let state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    assert!(fetch(&state, &format!("{}/pkg", server.uri()))
        .await
        .is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        state
            .circuit_breaker
            .health_snapshot(&state.repository.as_ref().unwrap().breaker_key())
            .unwrap()
            .status,
        "open"
    );
}

#[tokio::test]
async fn metadata_negotiation_and_coalescing_keep_one_absolute_deadline() {
    let _lock = crate::test_env::env_lock();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("metadata")
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let context = context(&server.uri(), |_| {}).await;
    let mut state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    begin_metadata(&mut state);
    let original = state.metadata_deadline;
    assert!(fetch(&state, &format!("{}/json", server.uri()))
        .await
        .is_ok());
    begin_metadata(&mut state);
    assert_eq!(state.metadata_deadline, original);
    assert!(metadata_wait_budget(&state, 30) < Duration::from_millis(600));
    assert!(fetch(&state, &format!("{}/html", server.uri()))
        .await
        .is_err());
    assert_eq!(metadata_wait_budget(&state, 30), Duration::ZERO);
}

#[tokio::test]
async fn a_metadata_body_stall_is_inside_the_operation_deadline() {
    let _lock = crate::test_env::env_lock();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nx")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let context = context(&uri, |_| {}).await;
    let state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    let started = std::time::Instant::now();
    assert!(fetch(&state, &uri).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
    server.abort();
}

#[tokio::test]
async fn connect_timeout_bounds_a_stalled_tls_handshake() {
    let _lock = crate::test_env::env_lock();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("https://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let context = context(&uri, |config| first_policy(config).metadata_timeout = 10).await;
    let state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    let client = ProxyClient::for_state(&state);
    let started = std::time::Instant::now();
    let error = client.send(client.get(&uri)).await.unwrap_err();
    assert!(error.is_timeout());
    assert!(started.elapsed() < Duration::from_secs(3));
    server.abort();
}

#[tokio::test]
async fn inactivity_timeout_also_applies_to_reused_connections() {
    let _lock = crate::test_env::env_lock();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let (seen, second) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        head(&mut stream).await;
        let _ = seen.send(());
        std::future::pending::<()>().await;
    });
    let context = context(&uri, |config| {
        let policy = first_policy(config);
        policy.metadata_timeout = 10;
        policy.read_timeout = 1;
    })
    .await;
    let state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    let client = ProxyClient::for_state(&state);
    assert_eq!(
        client
            .send(client.get(&uri))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"ok"
    );
    let started = std::time::Instant::now();
    let error = client.send(client.get(&uri)).await.unwrap_err();
    assert!(error.is_timeout());
    assert!(started.elapsed() < Duration::from_secs(3));
    second.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn unlimited_files_ignore_the_legacy_total_but_nonzero_caps_still_apply() {
    let _lock = crate::test_env::env_lock();
    for (total, chunks, succeeds) in [(0, 6, true), (1, 30, false)] {
        let (uri, server) = progressing(chunks).await;
        let context = context(&uri, |config| {
            let policy = first_policy(config);
            policy.total_timeout = total;
            policy.read_timeout = 1;
        })
        .await;
        let mut state = context
            .state
            .repositories
            .get("first")
            .unwrap()
            .state(&context.state);
        state.metadata_deadline = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        let result = proxy_fetch_artifact(
            &state,
            &uri,
            Duration::from_millis(25),
            None,
            &state.circuit_breaker,
            BreakerScope::upstream(RegistryType::Npm, &uri),
        )
        .await;
        assert_eq!(result.is_ok(), succeeds);
        if let Ok(bytes) = result {
            assert_eq!(bytes, b"data".repeat(chunks));
        }
        server.abort();
    }
}

#[tokio::test]
async fn slow_audit_error_bodies_do_not_release_a_probe_before_completion() {
    let _lock = crate::test_env::env_lock();
    for cancel in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            head(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 8\r\n\r\nx")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let context = context(&uri, |config| {
            first_policy(config).metadata_timeout = if cancel { 10 } else { 1 }
        })
        .await;
        let state = context
            .state
            .repositories
            .get("first")
            .unwrap()
            .state(&context.state);
        let breaker = &state.circuit_breaker;
        let key = state.repository.as_ref().unwrap().breaker_key();
        breaker.record_failure(&key, breaker.check(&key).unwrap());
        breaker.expire_open_for_test(&key);
        let metric = crate::metrics::UPSTREAM_REQUEST_DURATION.with_label_values(&["npm", "4xx"]);
        let before = metric.get_sample_count();
        let mut operation = Box::pin(proxy_forward_post(
            &state,
            &uri,
            Duration::from_secs(30),
            None,
            &[],
            b"{}",
            breaker,
            BreakerScope::upstream(RegistryType::Npm, &uri),
        ));
        let headers_seen = async {
            while metric.get_sample_count() == before {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            result = &mut operation => panic!("audit completed before stalled body: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), headers_seen) => result.unwrap(),
        }
        assert_eq!(breaker.health_snapshot(&key).unwrap().status, "half_open");
        if cancel {
            drop(operation);
        } else {
            assert!(operation.await.is_err());
        }
        assert_eq!(breaker.health_snapshot(&key).unwrap().status, "open");
        server.abort();
    }
}

#[tokio::test]
async fn cancelled_and_already_expired_operations_release_half_open_probes() {
    let _lock = crate::test_env::env_lock();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stall"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let context = context(&server.uri(), |config| {
        first_policy(config).metadata_timeout = 10
    })
    .await;
    let mut state = context
        .state
        .repositories
        .get("first")
        .unwrap()
        .state(&context.state);
    let breaker = state.circuit_breaker.clone();
    let key = state.repository.as_ref().unwrap().breaker_key();
    breaker.record_failure(&key, breaker.check(&key).unwrap());
    breaker.expire_open_for_test(&key);
    assert!(tokio::time::timeout(
        Duration::from_millis(50),
        fetch(&state, &format!("{}/stall", server.uri()))
    )
    .await
    .is_err());
    assert_eq!(breaker.health_snapshot(&key).unwrap().status, "open");
    breaker.expire_open_for_test(&key);
    state.metadata_deadline = Some(tokio::time::Instant::now() - Duration::from_secs(1));
    assert!(fetch(&state, &format!("{}/stall", server.uri()))
        .await
        .is_err());
    assert_eq!(breaker.health_snapshot(&key).unwrap().status, "open");
    breaker.expire_open_for_test(&key);
    let probe = breaker.check(&key).unwrap();
    breaker.record_success(&key, probe);
    assert_eq!(breaker.health_snapshot(&key).unwrap().status, "closed");
}
