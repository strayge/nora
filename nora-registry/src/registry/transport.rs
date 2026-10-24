// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! TLS exceptions belong to a configured origin, never to its redirect destinations.

use super::{ProxyError, Revalidation, Validators};
use crate::circuit_breaker::{BreakerScope, CircuitBreakerRegistry, ProbeToken};
use crate::config::repositories::ProxyRepository;
use crate::config::UpstreamAuth;
use crate::AppState;
use reqwest::header::{
    AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, LOCATION,
    PROXY_AUTHORIZATION,
};
use reqwest::{Client, Method, Request, RequestBuilder, Response, Url};
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

pub(crate) struct RepositoryTransport {
    origin: String,
    secure: bool,
    configured: Client,
    strict: Client,
    configured_raw: Client,
    strict_raw: Client,
}

#[derive(Debug)]
pub(super) enum RequestError {
    Network(reqwest::Error),
    Policy(&'static str),
}

impl RequestError {
    pub(super) fn is_timeout(&self) -> bool {
        matches!(self, Self::Network(error) if error.is_timeout())
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(error) => error.fmt(formatter),
            Self::Policy(reason) => formatter.write_str(reason),
        }
    }
}

fn certificates(path: &str) -> Result<Vec<reqwest::Certificate>, String> {
    let bytes = std::fs::read(path).map_err(|_| "cannot read TLS CA certificate")?;
    let certificates =
        reqwest::Certificate::from_pem_bundle(&bytes).map_err(|_| "invalid TLS CA certificate")?;
    if certificates.is_empty() {
        return Err("empty TLS CA certificate bundle".into());
    }
    Ok(certificates)
}

impl RepositoryTransport {
    pub(crate) fn new(
        policy: &ProxyRepository,
        global: &crate::config::TlsConfig,
    ) -> Result<Self, String> {
        let source = Url::parse(&policy.upstream).map_err(|_| "invalid upstream URL")?;
        let global_roots = global
            .ca_cert
            .as_deref()
            .map(certificates)
            .transpose()?
            .unwrap_or_default();
        let repository_roots = policy
            .tls_ca_cert
            .as_deref()
            .map(certificates)
            .transpose()?
            .unwrap_or_default();
        let base = |raw: bool| {
            let mut builder = Client::builder()
                .user_agent(crate::USER_AGENT)
                .connect_timeout(Duration::from_secs(policy.connect_timeout))
                .read_timeout(Duration::from_secs(policy.read_timeout))
                .redirect(reqwest::redirect::Policy::none());
            if raw {
                builder = builder.no_gzip().no_brotli().no_deflate().no_zstd();
            }
            for root in &global_roots {
                builder = builder.add_root_certificate(root.clone());
            }
            builder
        };
        let configured = |raw| {
            let mut builder = base(raw).danger_accept_invalid_certs(!policy.tls_verify);
            for certificate in &repository_roots {
                builder = builder.add_root_certificate(certificate.clone());
            }
            builder
                .build()
                .map_err(|_| "cannot build origin TLS client")
        };
        Ok(Self {
            origin: source.origin().ascii_serialization(),
            secure: source.scheme() == "https",
            configured: configured(false)?,
            strict: base(false)
                .build()
                .map_err(|_| "cannot build strict TLS client")?,
            configured_raw: configured(true)?,
            strict_raw: base(true)
                .build()
                .map_err(|_| "cannot build strict TLS client")?,
        })
    }

    async fn execute(&self, request: Request) -> Result<Response, RequestError> {
        self.execute_with(request, false).await
    }

    async fn execute_with(
        &self,
        mut request: Request,
        raw: bool,
    ) -> Result<Response, RequestError> {
        let mut seen = HashSet::new();
        let mut credentials_allowed = true;
        for hop in 0..=10 {
            if !matches!(request.url().scheme(), "http" | "https")
                || (self.secure && request.url().scheme() != "https")
            {
                return Err(RequestError::Policy("unsupported upstream URL scheme"));
            }
            let mut current = request.url().clone();
            let _ = current.set_username("");
            let _ = current.set_password(None);
            current.set_fragment(None);
            *request.url_mut() = current.clone();
            if !seen.insert(current.to_string()) {
                return Err(RequestError::Policy("upstream redirect loop"));
            }
            let configured_origin = current.origin().ascii_serialization() == self.origin;
            if !configured_origin {
                credentials_allowed = false;
            }
            if !credentials_allowed {
                for header in [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION] {
                    request.headers_mut().remove(header);
                }
            }
            let next = request.try_clone();
            let client = match (configured_origin, raw) {
                (true, false) => &self.configured,
                (false, false) => &self.strict,
                (true, true) => &self.configured_raw,
                (false, true) => &self.strict_raw,
            };
            let response = client
                .execute(request)
                .await
                .map_err(|error| RequestError::Network(error.without_url()))?;
            let status = response.status().as_u16();
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(LOCATION) else {
                return Ok(response);
            };
            if hop == 10 {
                return Err(RequestError::Policy("too many upstream redirects"));
            }
            let target = current
                .join(
                    location
                        .to_str()
                        .map_err(|_| RequestError::Policy("invalid upstream redirect"))?,
                )
                .map_err(|_| RequestError::Policy("invalid upstream redirect URL"))?;
            if !matches!(target.scheme(), "http" | "https")
                || (current.scheme() == "https" && target.scheme() != "https")
            {
                return Err(RequestError::Policy("unsafe upstream redirect scheme"));
            }
            request = next.ok_or(RequestError::Policy(
                "upstream redirect requires a replayable request",
            ))?;
            if (status == 303 && request.method() != Method::HEAD)
                || (matches!(status, 301 | 302) && request.method() == Method::POST)
            {
                *request.method_mut() = Method::GET;
                *request.body_mut() = None;
                for header in [CONTENT_LENGTH, CONTENT_TYPE, CONTENT_ENCODING] {
                    request.headers_mut().remove(header);
                }
            }
            *request.url_mut() = target;
        }
        Err(RequestError::Policy("too many upstream redirects"))
    }
}

fn deadline(seconds: u64) -> Option<tokio::time::Instant> {
    if seconds == 0 {
        return None;
    }
    // An unrepresentably distant deadline cannot expire during this process.
    tokio::time::Instant::now().checked_add(Duration::from_secs(seconds))
}

fn policy(state: &AppState) -> Option<&ProxyRepository> {
    match &state.repository.as_ref()?.definition {
        crate::config::repositories::RepositoryConfig::Proxy(policy) => Some(policy),
        _ => None,
    }
}

pub(crate) fn begin_metadata(state: &mut AppState) {
    if state.metadata_deadline.is_none() {
        state.metadata_deadline =
            policy(state).and_then(|policy| deadline(policy.metadata_timeout));
    }
}

pub(crate) fn metadata_wait_budget(state: &AppState, legacy_timeout: u64) -> Duration {
    match ProxyClient::for_state(state) {
        ProxyClient::Named {
            deadline: Some(deadline),
            ..
        } => deadline.saturating_duration_since(tokio::time::Instant::now()),
        _ => crate::proxy_coalesce::follower_budget(legacy_timeout),
    }
}

pub(crate) async fn proxy_fetch_artifact(
    state: &AppState,
    url: &str,
    timeout: Duration,
    auth: Option<UpstreamAuth<'_>>,
    cb: &CircuitBreakerRegistry,
    scope: impl Into<BreakerScope<'_>>,
) -> Result<Vec<u8>, ProxyError> {
    super::proxy_fetch_core(
        ProxyClient::for_artifact(state),
        url,
        timeout,
        auth,
        None,
        |response| async { response.bytes().await.map(|bytes| bytes.to_vec()) },
        cb,
        scope,
    )
    .await
}

pub(crate) async fn proxy_fetch(
    state: &AppState,
    url: &str,
    timeout: Duration,
    auth: Option<UpstreamAuth<'_>>,
    cb: &CircuitBreakerRegistry,
    scope: impl Into<BreakerScope<'_>>,
) -> Result<Vec<u8>, ProxyError> {
    super::proxy_fetch_core(
        ProxyClient::for_state(state),
        url,
        timeout,
        auth,
        None,
        |response| async { response.bytes().await.map(|bytes| bytes.to_vec()) },
        cb,
        scope,
    )
    .await
}

pub(crate) async fn proxy_fetch_text(
    state: &AppState,
    url: &str,
    timeout: Duration,
    auth: Option<UpstreamAuth<'_>>,
    extra_headers: Option<(&str, &str)>,
    cb: &CircuitBreakerRegistry,
    scope: impl Into<BreakerScope<'_>>,
) -> Result<String, ProxyError> {
    super::proxy_fetch_core(
        ProxyClient::for_state(state),
        url,
        timeout,
        auth,
        extra_headers,
        |response| response.text(),
        cb,
        scope,
    )
    .await
}

pub(crate) async fn proxy_fetch_conditional(
    state: &AppState,
    url: &str,
    timeout: Duration,
    auth: Option<UpstreamAuth<'_>>,
    validators: &Validators,
    cb: &CircuitBreakerRegistry,
    scope: impl Into<BreakerScope<'_>>,
) -> Result<Revalidation, ProxyError> {
    super::proxy_fetch_conditional_client(
        ProxyClient::for_state(state),
        url,
        timeout,
        auth,
        validators,
        cb,
        scope,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn proxy_forward_post(
    state: &AppState,
    url: &str,
    timeout: Duration,
    auth: Option<UpstreamAuth<'_>>,
    headers: &[(&str, &str)],
    body: &[u8],
    cb: &CircuitBreakerRegistry,
    scope: impl Into<BreakerScope<'_>>,
) -> Result<(u16, Vec<u8>, Option<String>), ProxyError> {
    if let ProxyClient::Legacy(client) = ProxyClient::for_state(state) {
        return super::proxy_forward_post(client, url, timeout, auth, headers, body, cb, scope)
            .await;
    }
    super::proxy_forward_post_client(
        ProxyClient::for_state(state),
        url,
        timeout,
        auth,
        headers,
        body,
        cb,
        scope,
    )
    .await
}

struct ProbeGuard<'a> {
    breaker: &'a CircuitBreakerRegistry,
    key: &'a str,
    probe: ProbeToken,
    finished: bool,
}

impl ProbeGuard<'_> {
    fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.record_failure(self.key, self.probe);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum ProxyClient<'a> {
    Legacy(&'a Client),
    Named {
        transport: &'a RepositoryTransport,
        repository: &'a crate::repositories::Repository,
        deadline: Option<tokio::time::Instant>,
        raw: bool,
    },
}

impl<'a> ProxyClient<'a> {
    pub(super) fn for_state(state: &'a AppState) -> Self {
        Self::for_operation(state, false, false)
    }
    pub(super) fn for_artifact(state: &'a AppState) -> Self {
        Self::for_operation(state, true, false)
    }
    pub(super) fn for_raw_artifact(state: &'a AppState) -> Self {
        Self::for_operation(state, true, true)
    }

    fn for_operation(state: &'a AppState, artifact: bool, raw: bool) -> Self {
        if let Some(repository) = state.repository.as_ref() {
            if let (Some(transport), Some(policy)) = (repository.transport.as_ref(), policy(state))
            {
                return Self::Named {
                    transport,
                    repository,
                    raw,
                    deadline: if artifact {
                        deadline(policy.total_timeout)
                    } else {
                        state
                            .metadata_deadline
                            .or_else(|| deadline(policy.metadata_timeout))
                    },
                };
            }
        }
        Self::Legacy(&state.http_client)
    }

    pub(super) fn breaker_key(self, scope: BreakerScope<'_>) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Legacy(_) => scope.key(),
            Self::Named { repository, .. } => std::borrow::Cow::Owned(repository.breaker_key()),
        }
    }

    pub(super) async fn run<T>(
        self,
        breaker: &CircuitBreakerRegistry,
        key: &str,
        probe: ProbeToken,
        operation: impl std::future::Future<Output = Result<T, ProxyError>>,
    ) -> Result<T, ProxyError> {
        let Self::Named {
            deadline,
            repository,
            ..
        } = self
        else {
            return operation.await;
        };
        let guard = ProbeGuard {
            breaker,
            key,
            probe,
            finished: false,
        };
        let guarded = async move {
            let result = operation.await;
            guard.finish();
            result
        };
        if let Some(deadline) = deadline {
            let started = std::time::Instant::now();
            match tokio::time::timeout_at(deadline, guarded).await {
                Ok(result) => result,
                Err(_) => {
                    crate::metrics::UPSTREAM_REQUEST_DURATION
                        .with_label_values(&[repository.registry_type().as_str(), "timeout"])
                        .observe(started.elapsed().as_secs_f64());
                    Err(ProxyError::Network("repository operation timed out".into()))
                }
            }
        } else {
            guarded.await
        }
    }

    pub(super) fn diagnostic_url(self, url: &str) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Legacy(_) => std::borrow::Cow::Borrowed(url),
            Self::Named { repository, .. } => std::borrow::Cow::Owned(repository.breaker_key()),
        }
    }

    fn template(self) -> &'a Client {
        match self {
            Self::Legacy(client) => client,
            Self::Named {
                transport,
                raw: true,
                ..
            } => &transport.strict_raw,
            Self::Named { transport, .. } => &transport.strict,
        }
    }

    pub(super) fn get(self, url: &str) -> RequestBuilder {
        self.template().get(url)
    }
    pub(super) fn post(self, url: &str) -> RequestBuilder {
        self.template().post(url)
    }

    pub(super) async fn send(self, builder: RequestBuilder) -> Result<Response, RequestError> {
        match self {
            Self::Legacy(_) => builder.send().await.map_err(RequestError::Network),
            Self::Named {
                transport,
                deadline,
                raw,
                ..
            } => {
                let mut request = builder
                    .build()
                    .map_err(|error| RequestError::Network(error.without_url()))?;
                *request.timeout_mut() = deadline.map(|deadline| {
                    deadline.saturating_duration_since(tokio::time::Instant::now())
                });
                if raw {
                    transport.execute_with(request, true).await
                } else {
                    transport.execute(request).await
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod timeouts_tests;
