// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! TLS exceptions belong to a configured origin, never to its redirect destinations.

use super::{ProxyError, Revalidation, Validators};
use crate::circuit_breaker::{BreakerScope, CircuitBreakerRegistry};
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
        let base = || {
            let mut builder = Client::builder()
                .user_agent(crate::USER_AGENT)
                .redirect(reqwest::redirect::Policy::none());
            for root in &global_roots {
                builder = builder.add_root_certificate(root.clone());
            }
            builder
        };
        let mut configured = base().danger_accept_invalid_certs(!policy.tls_verify);
        if let Some(path) = &policy.tls_ca_cert {
            for certificate in certificates(path)? {
                configured = configured.add_root_certificate(certificate);
            }
        }
        Ok(Self {
            origin: source.origin().ascii_serialization(),
            secure: source.scheme() == "https",
            configured: configured
                .build()
                .map_err(|_| "cannot build origin TLS client")?,
            strict: base()
                .build()
                .map_err(|_| "cannot build strict TLS client")?,
        })
    }

    async fn execute(&self, mut request: Request) -> Result<Response, RequestError> {
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
            let response = if configured_origin {
                &self.configured
            } else {
                &self.strict
            }
            .execute(request)
            .await
            .map_err(RequestError::Network)?;
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

#[derive(Clone, Copy)]
pub(super) enum ProxyClient<'a> {
    Legacy(&'a Client),
    Named(&'a RepositoryTransport),
}

impl<'a> ProxyClient<'a> {
    pub(super) fn for_state(state: &'a AppState) -> Self {
        match state
            .repository
            .as_ref()
            .and_then(|repository| repository.transport.as_ref())
        {
            Some(transport) => Self::Named(transport),
            None => Self::Legacy(&state.http_client),
        }
    }

    fn template(self) -> &'a Client {
        match self {
            Self::Legacy(client) => client,
            Self::Named(transport) => &transport.strict,
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
            Self::Named(transport) => {
                transport
                    .execute(builder.build().map_err(RequestError::Network)?)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests;
