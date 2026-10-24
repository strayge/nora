// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;
use rustls::pki_types::pem::PemObject;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::{rustls, TlsAcceptor};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/registry/transport/fixtures/ca.pem"
);

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: std::collections::HashMap<String, String>,
}

struct TlsServer {
    uri: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    plaintext: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn reply(status: u16, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (key, value) in headers {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    response.push_str("\r\n");
    let mut response = response.into_bytes();
    response.extend_from_slice(body);
    response
}

async fn serve(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    requests: &Mutex<Vec<Recorded>>,
    handler: &impl Fn(&Recorded) -> Vec<u8>,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 1024];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 1024 * 1024);
    };
    let text = String::from_utf8_lossy(&bytes[..end]);
    let mut lines = text.lines();
    let first = lines.next().unwrap();
    let mut coordinate = first.split_whitespace();
    let request = Recorded {
        method: coordinate.next().unwrap().into(),
        path: coordinate.next().unwrap().into(),
        headers: lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
            .collect(),
    };
    let length = request
        .headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut buffer = [0; 1024];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 1024 * 1024);
    }
    requests.lock().unwrap().push(request.clone());
    stream.write_all(&handler(&request)).await?;
    stream.shutdown().await
}

impl TlsServer {
    async fn start(handler: impl Fn(&Recorded) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let certificates = rustls::pki_types::CertificateDer::pem_slice_iter(include_bytes!(
            "fixtures/server.pem"
        ))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(include_bytes!(
            "fixtures/server-key.pem"
        ))
        .unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("https://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let plaintext = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let plain = plaintext.clone();
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut prefix = [0];
                if stream.peek(&mut prefix).await.is_err() {
                    continue;
                }
                if prefix[0] == 0x16 {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        let _ = serve(stream, &captured, &handler).await;
                    }
                } else {
                    plain.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = serve(stream, &captured, &handler).await;
                }
            }
        });
        Self {
            uri,
            requests,
            plaintext,
            task,
        }
    }
}

fn policy(url: &str, verify: bool, ca: bool) -> ProxyRepository {
    let mut policy: ProxyRepository = toml::from_str(&format!(
        "format='npm'\npaths=['/proxy/']\nupstream={url:?}\n"
    ))
    .unwrap();
    policy.tls_verify = verify;
    policy.tls_ca_cert = ca.then(|| CA.into());
    policy
}

async fn get(transport: &RepositoryTransport, url: &str) -> Result<Response, RequestError> {
    transport
        .execute(
            Client::new()
                .get(url)
                .basic_auth("alice", Some("secret"))
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
        )
        .await
}

#[tokio::test]
async fn global_ca_roots_remain_global_for_origins_and_redirects() {
    let _lock = crate::test_env::env_lock();
    let destination = TlsServer::start(|_| reply(200, &[], b"global trust")).await;
    let target = destination.uri.clone();
    let source = TlsServer::start(move |_| reply(302, &[("Location", target.clone())], b"")).await;
    let global = crate::config::TlsConfig {
        ca_cert: Some(CA.into()),
    };
    let destination_transport =
        RepositoryTransport::new(&policy(&destination.uri, true, false), &global).unwrap();
    assert!(get(&destination_transport, &destination.uri)
        .await
        .unwrap()
        .status()
        .is_success());
    let transport = RepositoryTransport::new(&policy(&source.uri, true, false), &global).unwrap();
    assert_eq!(
        get(&transport, &source.uri)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"global trust"
    );
    let requests = destination.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers.contains_key("authorization"));
    assert!(!requests[1].headers.contains_key("authorization"));
}

#[tokio::test]
async fn custom_ca_and_insecure_policies_are_isolated_between_repositories() {
    let _lock = crate::test_env::env_lock();
    let server = TlsServer::start(|_| reply(200, &[], b"trusted source")).await;
    let default =
        RepositoryTransport::new(&policy(&server.uri, true, false), &Default::default()).unwrap();
    assert!(get(&default, &server.uri).await.is_err());
    for (verify, ca) in [(true, true), (false, false)] {
        let transport =
            RepositoryTransport::new(&policy(&server.uri, verify, ca), &Default::default())
                .unwrap();
        assert_eq!(
            get(&transport, &server.uri)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"trusted source"
        );
    }
    assert!(get(&default, &server.uri).await.is_err());
    let invalid = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(invalid.path(), b"not a certificate").unwrap();
    let mut invalid_policy = policy(&server.uri, true, false);
    invalid_policy.tls_ca_cert = Some(invalid.path().to_str().unwrap().into());
    assert!(RepositoryTransport::new(&invalid_policy, &Default::default()).is_err());
}

#[tokio::test]
async fn redirected_origins_never_inherit_custom_ca_or_insecure_verification() {
    let _lock = crate::test_env::env_lock();
    let destination = TlsServer::start(|_| reply(200, &[], b"must not be trusted")).await;
    let target = destination.uri.clone();
    let source = TlsServer::start(move |_| reply(302, &[("Location", target.clone())], b"")).await;
    for (verify, ca) in [(true, true), (false, false)] {
        let transport =
            RepositoryTransport::new(&policy(&source.uri, verify, ca), &Default::default())
                .unwrap();
        assert!(get(&transport, &source.uri).await.is_err());
    }
    assert!(destination.requests.lock().unwrap().is_empty());
    assert_eq!(source.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn same_host_same_port_https_downgrade_never_sends_plaintext_credentials() {
    let _lock = crate::test_env::env_lock();
    let source = TlsServer::start(|request| {
        reply(
            302,
            &[(
                "Location",
                format!("http://{}/leak", request.headers["host"]),
            )],
            b"",
        )
    })
    .await;
    let transport =
        RepositoryTransport::new(&policy(&source.uri, true, true), &Default::default()).unwrap();
    assert!(matches!(
        get(&transport, &source.uri).await,
        Err(RequestError::Policy(_))
    ));
    assert_eq!(
        source.plaintext.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let requests = source.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers["authorization"],
        "Basic YWxpY2U6c2VjcmV0"
    );
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/");
}

#[tokio::test]
async fn redirected_methods_and_cross_origin_credentials_are_explicit() {
    let _lock = crate::test_env::env_lock();
    let destination = MockServer::start().await;
    Mock::given(path("/target"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&destination)
        .await;
    let source = MockServer::start().await;
    for status in [301, 302, 303, 307, 308] {
        Mock::given(path(format!("/{status}")))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", format!("{}/target", destination.uri())),
            )
            .mount(&source)
            .await;
    }
    let transport =
        RepositoryTransport::new(&policy(&source.uri(), true, false), &Default::default()).unwrap();
    for status in [301, 302, 303, 307, 308] {
        let response = transport
            .execute(
                Client::new()
                    .post(format!("{}/{status}", source.uri()))
                    .basic_auth("alice", Some("secret"))
                    .body("audit-body")
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    let requests = destination.received_requests().await.unwrap();
    for (request, status) in requests.iter().zip([301, 302, 303, 307, 308]) {
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.method.as_str(),
            if status >= 307 { "POST" } else { "GET" }
        );
        assert_eq!(
            request.body.as_slice(),
            if status >= 307 {
                &b"audit-body"[..]
            } else {
                &b""[..]
            }
        );
    }
    transport
        .execute(
            Client::new()
                .head(format!("{}/303", source.uri()))
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        destination
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .method
            .as_str(),
        "HEAD"
    );
}

#[tokio::test]
async fn relative_redirects_keep_origin_auth_but_loops_and_long_chains_stop() {
    let _lock = crate::test_env::env_lock();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/end"))
        .mount(&server)
        .await;
    Mock::given(path("/end"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let transport =
        RepositoryTransport::new(&policy(&server.uri(), true, false), &Default::default()).unwrap();
    assert!(get(&transport, &format!("{}/start", server.uri()))
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.headers["authorization"] == "Basic YWxpY2U6c2VjcmV0"));
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    assert!(matches!(
        get(&transport, &format!("{}/loop", server.uri())).await,
        Err(RequestError::Policy(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(|request: &wiremock::Request| {
            let index = request
                .url
                .path()
                .trim_start_matches('/')
                .parse::<u32>()
                .unwrap();
            ResponseTemplate::new(302).insert_header("location", format!("/{}", index + 1))
        })
        .mount(&server)
        .await;
    assert!(matches!(
        get(&transport, &format!("{}/0", server.uri())).await,
        Err(RequestError::Policy(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 11);
}

#[tokio::test]
async fn npm_and_pypi_metadata_and_files_use_the_same_origin_tls_policy() {
    let _lock = crate::test_env::env_lock();
    let server = TlsServer::start(|request| match request.path.as_str() {
        "/npm/pkg" if request.headers.contains_key("if-none-match") => reply(304, &[], b""),
        "/npm/pkg" => reply(200, &[("ETag", "\"tls-v1\"".into())], serde_json::to_string(&serde_json::json!({"name":"pkg", "versions":{"1.0.0":{"name":"pkg", "version":"1.0.0", "dist":{"tarball":format!("https://{}/npm/file.tgz", request.headers["host"])}}}})).unwrap().as_bytes()),
        "/npm/-/npm/v1/security/advisories/bulk" => reply(200, &[], b"{}"),
        "/simple/pkg/" => reply(200, &[], format!("<a href=\"https://{}/files/pkg-1.0.tar.gz\">pkg-1.0.tar.gz</a>", request.headers["host"]).as_bytes()),
        "/npm/file.tgz" | "/files/pkg-1.0.tar.gz" => reply(200, &[], b"tls-package"),
        _ => reply(404, &[], b""),
    }).await;
    let context = crate::test_helpers::with_named_repositories(
        crate::test_helpers::create_test_context_with_config(|config| {
            config.repositories = toml::from_str::<crate::config::Config>(&format!(
                r#"
[repositories.npm]
format = "npm"
kind = "proxy"
paths = ["/node/"]
upstream = "{}/npm"
auth = "alice:secret"
[repositories.python]
format = "pypi"
kind = "proxy"
paths = ["/python/"]
upstream = "{}/simple"
auth = "alice:secret"
"#,
                server.uri, server.uri
            ))
            .unwrap()
            .repositories;
            for repository in config.repositories.values_mut() {
                let crate::config::repositories::RepositoryConfig::Proxy(proxy) = repository else {
                    panic!()
                };
                proxy.tls_ca_cert = Some(CA.into());
            }
        }),
    )
    .await;
    for path in ["/node/pkg", "/python/pkg/"] {
        assert_eq!(
            crate::test_helpers::send(&context.app, axum::http::Method::GET, path, "")
                .await
                .status(),
            axum::http::StatusCode::OK,
            "{path}"
        );
    }
    for path in ["/node/pkg/-/pkg-1.0.0.tgz", "/python/pkg/pkg-1.0.tar.gz"] {
        let response =
            crate::test_helpers::send(&context.app, axum::http::Method::GET, path, "").await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
        assert_eq!(
            crate::test_helpers::body_bytes(response).await.as_ref(),
            b"tls-package"
        );
    }
    let state = context
        .state
        .repositories
        .get("npm")
        .unwrap()
        .state(&context.state);
    let upstream = format!("{}/npm", server.uri);
    let conditional = proxy_fetch_conditional(
        &state,
        &format!("{upstream}/pkg"),
        Duration::from_secs(3),
        Some(UpstreamAuth::Basic("alice:secret")),
        &Validators {
            etag: Some("\"tls-v1\"".into()),
            last_modified: None,
        },
        &state.circuit_breaker,
        BreakerScope::upstream(crate::registry_type::RegistryType::Npm, &upstream),
    )
    .await
    .unwrap();
    assert!(matches!(conditional, Revalidation::NotModified));
    let audit = proxy_forward_post(
        &state,
        &format!("{upstream}/-/npm/v1/security/advisories/bulk"),
        Duration::from_secs(3),
        Some(UpstreamAuth::Basic("alice:secret")),
        &[("content-type", "application/json")],
        b"{}",
        &state.circuit_breaker,
        BreakerScope::upstream(crate::registry_type::RegistryType::Npm, &upstream),
    )
    .await
    .unwrap();
    assert_eq!(audit.0, 200);
    assert!(server.requests.lock().unwrap().iter().all(|request| request
        .headers
        .get("authorization")
        .is_some_and(|value| value == "Basic YWxpY2U6c2VjcmV0")));
}
