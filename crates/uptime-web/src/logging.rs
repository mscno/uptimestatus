//! Request logging: one line per request, and every handler error with its
//! cause.
//!
//! Access lines carry `method`, `host`, `path` (never the query string, which
//! may hold OAuth codes), `status`, `latency_ms`, and behind a reverse proxy
//! `client_ip` and `request_id`. Failed requests log at ERROR. Health checks log at DEBUG
//! (the platform polls them every few seconds).

use std::{borrow::Cow, time::Instant};

use axum::{extract::Request, middleware::Next, response::Response};
use http::{HeaderMap, header::HOST};
use topcoat::{
    context::Cx,
    router::{
        Body, Layer, LayerFuture, Next as TopcoatNext, Path,
        error::{
            BadRequestError, ContentTooLargeError, ForbiddenError, MethodNotAllowedError,
            NotFoundError, RedirectError, SeeOther, TooManyRequestsError, UnauthorizedError,
        },
        request::parts,
    },
};
use tracing::Instrument as _;

/// The path as logged: secrets in the path (push tokens) are masked.
pub(crate) fn loggable_path(path: &str) -> Cow<'_, str> {
    match path.strip_prefix("/api/push/") {
        Some(token) if !token.is_empty() => Cow::Borrowed("/api/push/…"),
        _ => Cow::Borrowed(path),
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The visitor's address as a reverse proxy reports it: the first
/// `X-Forwarded-For` entry, else `X-Real-IP`. Logging only: the headers are
/// client-controlled unless a trusted proxy overwrites them.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    let forwarded = header(headers, "x-forwarded-for")
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty());
    forwarded
        .or_else(|| header(headers, "x-real-ip").map(str::trim))
        .filter(|ip| !ip.is_empty())
        .map(str::to_owned)
}

/// The proxy's request id (`X-Request-Id`), when it sets one.
fn request_id(headers: &HeaderMap) -> Option<String> {
    header(headers, "x-request-id").map(str::to_owned)
}

/// Axum middleware for the public listener: one line per request.
pub(crate) async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = loggable_path(request.uri().path()).into_owned();
    let host = header(request.headers(), HOST.as_str()).map(crate::host::normalize_host);
    // Set by a reverse proxy; absent when clients connect directly.
    let client_ip = client_ip(request.headers());
    let request_id = request_id(request.headers());
    // Anything logged while handling the request carries its id.
    let span = tracing::info_span!("request", id = request_id.as_deref());

    let response = next.run(request).instrument(span).await;

    let status = response.status().as_u16();
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    macro_rules! access {
        ($level:expr) => {
            tracing::event!(
                $level,
                %method,
                host = host.as_deref(),
                path = path.as_str(),
                status,
                latency_ms,
                client_ip = client_ip.as_deref(),
                request_id = request_id.as_deref(),
                "{method} {path} {status}"
            )
        };
    }
    if status >= 500 {
        access!(tracing::Level::ERROR);
    } else if path == "/healthz" {
        access!(tracing::Level::DEBUG);
    } else {
        access!(tracing::Level::INFO);
    }
    response
}

/// Axum middleware for the internal listener (probes and scrapes): failures
/// only, unless debugging.
pub(crate) async fn log_internal_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    if status >= 500 {
        tracing::error!(%method, path, status, "internal: {method} {path} {status}");
    } else {
        tracing::debug!(%method, path, status, "internal: {method} {path} {status}");
    }
    response
}

/// Topcoat layer logging handler errors that become a 500, with the whole
/// cause chain (Topcoat's response carries only "internal server error").
/// Expected outcomes (not found, redirects, bad input, auth) are not errors.
pub(crate) struct LogErrors;

impl Layer for LogErrors {
    fn path(&self) -> Option<&Path> {
        None
    }

    fn handle<'a>(&'a self, cx: &'a Cx, body: Body, next: TopcoatNext<'a>) -> LayerFuture<'a> {
        Box::pin(async move {
            let result = next.run(cx, body).await;
            if let Err(error) = &result
                && !expected(error)
            {
                let request = parts(cx);
                tracing::error!(
                    method = %request.method,
                    path = %loggable_path(request.uri.path()),
                    error = describe(error),
                    "request failed"
                );
            }
            result
        })
    }
}

/// A live stream's update, logged when it failed (the stream ends there and
/// the browser reconnects, so nothing else would tell).
pub(crate) fn live_update<T>(result: topcoat::Result<T>, stream: &str) -> topcoat::Result<T> {
    if let Err(error) = &result
        && !expected(error)
    {
        tracing::error!(stream, error = describe(error), "live update failed");
    }
    result
}

/// The error and its causes on one line.
fn describe(error: &topcoat::Error) -> String {
    uptime_domain::report::chain(error.chain().map(ToString::to_string))
}

/// Errors that are answers, not failures.
fn expected(error: &topcoat::Error) -> bool {
    error.is::<NotFoundError>()
        || error.is::<RedirectError>()
        || error.is::<SeeOther>()
        || error.is::<ForbiddenError>()
        || error.is::<UnauthorizedError>()
        || error.is::<BadRequestError>()
        || error.is::<MethodNotAllowedError>()
        || error.is::<ContentTooLargeError>()
        || error.is::<TooManyRequestsError>()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("/s/platform", "/s/platform")]
    #[case("/api/push/abc123", "/api/push/…")]
    #[case("/api/push/", "/api/push/")]
    #[case("/api/pushy", "/api/pushy")]
    fn secrets_in_paths_are_masked(#[case] path: &str, #[case] logged: &str) {
        assert_eq!(loggable_path(path), logged);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
            .collect()
    }

    #[rstest]
    #[case(&[("x-forwarded-for", "203.0.113.7")], Some("203.0.113.7"))]
    #[case(&[("x-forwarded-for", "203.0.113.7, 10.0.0.2, 10.0.0.3")], Some("203.0.113.7"))]
    #[case(&[("x-forwarded-for", " 2001:db8::1 ")], Some("2001:db8::1"))]
    #[case(&[("x-real-ip", "198.51.100.4")], Some("198.51.100.4"))]
    #[case(&[("x-forwarded-for", "203.0.113.7"), ("x-real-ip", "198.51.100.4")], Some("203.0.113.7"))]
    #[case(&[("x-forwarded-for", ""), ("x-real-ip", "198.51.100.4")], Some("198.51.100.4"))]
    #[case(&[], None)]
    fn the_client_address_comes_from_proxy_headers(
        #[case] pairs: &[(&str, &str)],
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(client_ip(&headers(pairs)).as_deref(), expected);
    }

    #[test]
    fn the_request_id_comes_from_the_proxy() {
        assert_eq!(
            request_id(&headers(&[("x-request-id", "abc-123")])).as_deref(),
            Some("abc-123")
        );
        assert_eq!(request_id(&headers(&[])), None);
    }
}
