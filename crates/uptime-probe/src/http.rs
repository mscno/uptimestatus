//! HTTP(S) probe.

use std::{
    collections::HashMap,
    error::Error as StdError,
    net::IpAddr,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use reqwest::{Client, Method, redirect};
use uptime_domain::{FailureKind, HttpAuth, HttpCheck, Observation};
use url::{Host, Url};

use crate::{
    AddressPolicy, SetupError,
    resolve::{PolicyResolver, ResolveError},
};

/// A redirect pointed at an address the policy forbids.
#[derive(Debug, thiserror::Error)]
#[error("redirect to non-public address {0} refused")]
struct BlockedRedirect(IpAddr);

#[derive(Debug, thiserror::Error)]
#[error("stopped after {0} redirects")]
struct TooManyRedirects(u8);

/// HTTP clients keyed by the settings that must live on the client itself.
#[derive(Debug)]
pub(crate) struct HttpProbe {
    resolver: PolicyResolver,
    user_agent: String,
    max_body_bytes: usize,
    clients: Mutex<HashMap<(bool, u8), Client>>,
}

impl HttpProbe {
    pub(crate) fn new(resolver: PolicyResolver, user_agent: String, max_body_bytes: usize) -> Self {
        Self {
            resolver,
            user_agent,
            max_body_bytes,
            clients: Mutex::default(),
        }
    }

    pub(crate) async fn probe(&self, check: &HttpCheck, timeout: Duration) -> Observation {
        if let Some(ip) = ip_literal(&check.url)
            && !self.resolver.policy().permits(ip)
        {
            return failed(
                FailureKind::Blocked,
                format!("{ip} is not a public address"),
            );
        }
        let client = match self.client(check.ignore_tls_errors, check.max_redirects) {
            Ok(client) => client,
            Err(error) => return failed(FailureKind::Io, error.to_string()),
        };

        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        let attempt = async {
            let mut request = client
                .request(method(check), check.url.clone())
                .timeout(timeout);
            for (name, value) in &check.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            request = match &check.auth {
                Some(HttpAuth::Basic { username, password }) => {
                    request.basic_auth(username, Some(password))
                }
                Some(HttpAuth::Bearer { token }) => request.bearer_auth(token),
                None => request,
            };
            if let Some(body) = &check.body {
                request = request.body(body.clone());
            }
            let response = request.send().await?;
            let latency = started.elapsed();
            let cert_expires_at = response
                .extensions()
                .get::<reqwest::tls::TlsInfo>()
                .and_then(reqwest::tls::TlsInfo::peer_certificate)
                .and_then(crate::cert::not_after);
            let status_code = response.status().as_u16();
            let response_body = if !check.accepted_status.contains(response.status().as_u16())
                || check.keyword.is_some()
                || check.json.is_some()
            {
                let limit = if check.keyword.is_some() || check.json.is_some() {
                    self.max_body_bytes
                } else {
                    self.max_body_bytes.min(4096)
                };
                let (body, error) = read_body(response, limit, deadline).await;
                if let Some((kind, message)) = error {
                    return Ok(Observation::FailedResponse {
                        latency,
                        status_code,
                        response_body: excerpt(body),
                        kind,
                        message,
                    });
                }
                Some(body)
            } else {
                None
            };
            let keyword_found = check.keyword.as_ref().map(|rule| {
                response_body
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&rule.text)
            });
            let json_matched = check
                .json
                .as_ref()
                .map(|rule| rule.matches(response_body.as_deref().unwrap_or_default()));
            Ok::<_, reqwest::Error>(Observation::Responded {
                latency,
                status_code: Some(status_code),
                keyword_found,
                json_matched,
                cert_expires_at,
                response_body: response_body.map(excerpt),
            })
        };

        match tokio::time::timeout(timeout, attempt).await {
            Ok(Ok(observation)) => observation,
            Ok(Err(error)) => {
                let (kind, message) = classify(&error);
                failed(kind, message)
            }
            Err(_) => failed(
                FailureKind::Timeout,
                format!("no response within {timeout:?}"),
            ),
        }
    }

    fn client(&self, ignore_tls_errors: bool, max_redirects: u8) -> Result<Client, SetupError> {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&(ignore_tls_errors, max_redirects)) {
            return Ok(client.clone());
        }
        let client = Client::builder()
            .user_agent(self.user_agent.clone())
            .dns_resolver(self.resolver.clone())
            .redirect(redirect_policy(self.resolver.policy(), max_redirects))
            // Measure a fresh connection every time; pooled connections hide outages.
            .pool_max_idle_per_host(0)
            .danger_accept_invalid_certs(ignore_tls_errors)
            // Exposes the peer certificate, for expiry warnings.
            .tls_info(true)
            .build()
            .map_err(|e| SetupError::new(format!("building the HTTP client: {e}")))?;
        clients.insert((ignore_tls_errors, max_redirects), client.clone());
        Ok(client)
    }
}

fn redirect_policy(policy: AddressPolicy, max_redirects: u8) -> redirect::Policy {
    if max_redirects == 0 {
        return redirect::Policy::none();
    }
    redirect::Policy::custom(move |attempt| {
        if let Some(ip) = ip_literal(attempt.url())
            && !policy.permits(ip)
        {
            return attempt.error(BlockedRedirect(ip));
        }
        if attempt.previous().len() > usize::from(max_redirects) {
            attempt.error(TooManyRedirects(max_redirects))
        } else {
            attempt.follow()
        }
    })
}

fn method(check: &HttpCheck) -> Method {
    Method::from_bytes(check.method.as_str().as_bytes()).unwrap_or(Method::GET)
}

fn ip_literal(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

/// Reads at most `limit` bytes of the body, as text.
async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
    deadline: tokio::time::Instant,
) -> (String, Option<(FailureKind, String)>) {
    let mut body = Vec::new();
    let mut failure = None;
    while body.len() < limit {
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let take = chunk.len().min(limit - body.len());
                body.extend_from_slice(&chunk[..take]);
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                failure = Some(classify(&error));
                break;
            }
            Err(_) => {
                failure = Some((
                    FailureKind::Timeout,
                    "timed out reading response body".into(),
                ));
                break;
            }
        }
    }
    (String::from_utf8_lossy(&body).into_owned(), failure)
}

fn excerpt(mut text: String) -> String {
    let mut end = text.len().min(4096);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn failed(kind: FailureKind, message: String) -> Observation {
    Observation::Failed { kind, message }
}

/// Maps a request error to a [`FailureKind`] and a readable message.
fn classify(error: &reqwest::Error) -> (FailureKind, String) {
    let message = describe(error);
    if error.is_timeout() {
        return (FailureKind::Timeout, message);
    }
    let mut cause: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(current) = cause {
        if let Some(resolve) = current.downcast_ref::<ResolveError>() {
            let kind = match resolve {
                ResolveError::Blocked { .. } => FailureKind::Blocked,
                ResolveError::Lookup { .. } => FailureKind::Dns,
            };
            return (kind, resolve.to_string());
        }
        if current.is::<BlockedRedirect>() {
            return (FailureKind::Blocked, current.to_string());
        }
        if current.is::<TooManyRedirects>() {
            return (FailureKind::Io, current.to_string());
        }
        if current.is::<rustls::Error>() {
            return (FailureKind::Tls, message);
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if wraps_tls_error(io) {
                return (FailureKind::Tls, message);
            }
            use std::io::ErrorKind::*;
            match io.kind() {
                ConnectionRefused | ConnectionReset | ConnectionAborted => {
                    return (FailureKind::Refused, message);
                }
                TimedOut => return (FailureKind::Timeout, message),
                _ => {}
            }
        }
        cause = current.source();
    }
    (FailureKind::Io, message)
}

/// Whether a (possibly nested) `io::Error` carries a TLS error.
///
/// `io::Error::source()` skips custom inner errors, and hyper wraps rustls's
/// error in two layers of `io::Error`, so unwrap them explicitly.
fn wraps_tls_error(error: &std::io::Error) -> bool {
    match error.get_ref() {
        Some(inner) if inner.is::<rustls::Error>() => true,
        Some(inner) => inner
            .downcast_ref::<std::io::Error>()
            .is_some_and(wraps_tls_error),
        None => false,
    }
}

/// The error and its causes, skipping reqwest's "error sending request" wrapper text.
fn describe(error: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut cause = error.source();
    while let Some(current) = cause {
        let text = current.to_string();
        if !parts.iter().any(|seen: &String| seen.contains(&text)) {
            parts.push(text);
        }
        cause = current.source();
    }
    if parts.is_empty() {
        error.to_string()
    } else {
        parts.join(": ")
    }
}
