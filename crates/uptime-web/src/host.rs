//! Host-based routing: the single place that decides what a hostname may reach.
//!
//! - `APP_HOST` reaches everything (admin console, auth, status pages).
//! - A verified custom domain reaches only its own status page: `/x` is
//!   rewritten to `/s/{slug}/x` before routing. The rewrite is idempotent, so
//!   links generated as `/s/{slug}/...` keep working on the custom domain.
//! - `EDGE_HOST` redirects to `APP_HOST`.
//! - Every other host gets a 404.
//! - `/healthz` answers on any host (platform health checks send arbitrary hosts).

use std::{
    collections::HashMap,
    sync::{Arc, PoisonError, RwLock},
};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use http::{StatusCode, Uri, header::HOST, uri::PathAndQuery};
use uptime_runtime::DomainSink;
use uptime_store::{Store, StoreError};
use url::Url;

use crate::AppState;

/// The hostnames this deployment answers to.
#[derive(Clone, Debug)]
pub struct Hosts {
    app_url: Url,
    app_host: String,
    edge_host: String,
}

impl Hosts {
    /// `app_url` is the admin console's public URL; its host is `APP_HOST`.
    pub fn new(app_url: Url, edge_host: &str) -> Self {
        let app_host = normalize_host(app_url.host_str().unwrap_or_default());
        Self {
            app_url,
            app_host,
            edge_host: normalize_host(edge_host),
        }
    }

    pub fn app_host(&self) -> &str {
        &self.app_host
    }

    pub fn app_url(&self) -> &Url {
        &self.app_url
    }

    pub fn edge_host(&self) -> &str {
        &self.edge_host
    }
}

/// Verified custom domains and the status-page slug each one serves.
///
/// Read by the host router; reloaded when domains change.
#[derive(Clone, Debug, Default)]
pub struct DomainCache {
    inner: Arc<RwLock<HashMap<String, String>>>,
}

impl DomainCache {
    /// Replaces every entry (`hostname → slug`).
    pub fn replace(&self, domains: impl IntoIterator<Item = (String, String)>) {
        let map = domains
            .into_iter()
            .map(|(host, slug)| (normalize_host(&host), slug))
            .collect();
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = map;
    }

    /// A [`DomainSink`] that replaces this cache's entries.
    pub fn sink(&self) -> DomainSink {
        let cache = self.clone();
        Arc::new(move |domains| {
            cache.replace(
                domains
                    .into_iter()
                    .map(|(host, slug)| (host.to_string(), slug.to_string())),
            );
        })
    }

    /// Reloads every verified domain from the store.
    pub async fn reload(&self, store: &Store) -> Result<usize, StoreError> {
        let domains = store.verified_domains().await?;
        let count = domains.len();
        (self.sink())(domains);
        Ok(count)
    }

    /// The slug served on `host`, if it is a verified custom domain.
    pub fn slug_for(&self, host: &str) -> Option<String> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&normalize_host(host))
            .cloned()
    }
}

/// Marks a request that arrived on a verified custom domain, where a status
/// page lives at the root: its links leave out `/s/{slug}`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CustomDomain;

/// What to do with a request, decided from its host and path alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRoute {
    /// Route the request as-is.
    Pass,
    /// Route the request with this path (query string preserved).
    Rewrite(String),
    /// Permanently redirect to this URL.
    Redirect(String),
    NotFound,
}

/// Decides how a request for `host` + `path_and_query` is routed.
pub fn route(
    hosts: &Hosts,
    domains: &DomainCache,
    host: Option<&str>,
    path_and_query: &str,
) -> HostRoute {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    if path == "/healthz" {
        return HostRoute::Pass;
    }
    let Some(host) = host.map(normalize_host) else {
        return HostRoute::NotFound;
    };

    if host == hosts.app_host {
        HostRoute::Pass
    } else if host == hosts.edge_host {
        let base = hosts.app_url.as_str().trim_end_matches('/');
        HostRoute::Redirect(format!("{base}{path_and_query}"))
    } else if let Some(slug) = domains.slug_for(&host) {
        // Shared assets and uploaded images (logos) load on custom domains too.
        if path.starts_with("/static/") || path.starts_with("/media/") {
            return HostRoute::Pass;
        }
        let prefix = format!("/s/{slug}");
        let already_scoped = path
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        if already_scoped {
            HostRoute::Pass
        } else if path_and_query == "/" || path_and_query.starts_with("/?") {
            HostRoute::Rewrite(format!("{prefix}{}", &path_and_query[1..]))
        } else {
            HostRoute::Rewrite(format!("{prefix}{path_and_query}"))
        }
    } else {
        HostRoute::NotFound
    }
}

/// Lowercases and strips any port and trailing dot: `Status.Example.com.:443` → `status.example.com`.
pub fn normalize_host(host: &str) -> String {
    let without_port = match host.rsplit_once(':') {
        // Bracketed IPv6 literal, possibly with a port: keep the brackets' content.
        _ if host.starts_with('[') => host
            .split(']')
            .next()
            .unwrap_or(host)
            .trim_start_matches('['),
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    without_port.trim_end_matches('.').to_ascii_lowercase()
}

/// Axum middleware applying [`route`] before the request reaches the router.
pub(crate) async fn route_by_host(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        })
        .map(str::to_owned);
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", PathAndQuery::as_str)
        .to_owned();

    let decision = route(
        &state.hosts,
        &state.domains,
        host.as_deref(),
        &path_and_query,
    );
    if let Some(host) = host.as_deref()
        && matches!(decision, HostRoute::Pass | HostRoute::Rewrite(_))
        && normalize_host(host) != state.hosts.app_host
        && state.domains.slug_for(host).is_some()
    {
        request.extensions_mut().insert(CustomDomain);
    }
    match decision {
        HostRoute::Pass => next.run(request).await,
        HostRoute::Rewrite(path_and_query) => {
            let mut parts = request.uri().clone().into_parts();
            parts.path_and_query = path_and_query.parse().ok();
            match Uri::from_parts(parts) {
                Ok(uri) => {
                    *request.uri_mut() = uri;
                    next.run(request).await
                }
                Err(_) => StatusCode::BAD_REQUEST.into_response(),
            }
        }
        HostRoute::Redirect(location) => Redirect::permanent(&location).into_response(),
        HostRoute::NotFound => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    fn hosts() -> Hosts {
        Hosts::new(
            "https://status.example.com".parse().unwrap(),
            "edge.example.com",
        )
    }

    fn domains() -> DomainCache {
        let cache = DomainCache::default();
        cache.replace([("status.team.dev".to_owned(), "platform".to_owned())]);
        cache
    }

    fn decide(host: Option<&str>, path: &str) -> HostRoute {
        route(&hosts(), &domains(), host, path)
    }

    #[rstest]
    #[case("/")]
    #[case("/admin")]
    #[case("/s/platform")]
    #[case("/auth/github/callback?code=1&state=2")]
    fn app_host_reaches_everything(#[case] path: &str) {
        assert_eq!(decide(Some("status.example.com"), path), HostRoute::Pass);
    }

    #[test]
    fn app_host_matching_ignores_case_port_and_trailing_dot() {
        assert_eq!(
            decide(Some("STATUS.Example.com.:443"), "/admin"),
            HostRoute::Pass
        );
    }

    #[rstest]
    #[case("/", "/s/platform")]
    #[case("/incidents/7", "/s/platform/incidents/7")]
    #[case("/summary.json?pretty=1", "/s/platform/summary.json?pretty=1")]
    #[case("/admin", "/s/platform/admin")]
    #[case("/s/other", "/s/platform/s/other")]
    fn custom_domain_is_confined_to_its_status_page(#[case] path: &str, #[case] rewritten: &str) {
        assert_eq!(
            decide(Some("status.team.dev"), path),
            HostRoute::Rewrite(rewritten.to_owned())
        );
    }

    #[rstest]
    #[case("/s/platform")]
    #[case("/s/platform/incidents/7")]
    fn custom_domain_rewrite_is_idempotent(#[case] path: &str) {
        assert_eq!(decide(Some("status.team.dev"), path), HostRoute::Pass);
    }

    #[test]
    fn custom_domains_can_load_static_assets() {
        assert_eq!(
            decide(Some("status.team.dev"), "/static/app.css?v=1"),
            HostRoute::Pass
        );
    }

    #[test]
    fn custom_domains_can_load_uploaded_images() {
        assert_eq!(
            decide(
                Some("status.team.dev"),
                "/media/0123456789abcdef0123456789abcdef.png"
            ),
            HostRoute::Pass
        );
    }

    #[test]
    fn slug_prefix_must_end_at_a_segment_boundary() {
        assert_eq!(
            decide(Some("status.team.dev"), "/s/platformx"),
            HostRoute::Rewrite("/s/platform/s/platformx".to_owned())
        );
    }

    #[test]
    fn edge_host_redirects_to_the_app() {
        assert_eq!(
            decide(Some("edge.example.com"), "/s/platform?x=1"),
            HostRoute::Redirect("https://status.example.com/s/platform?x=1".to_owned())
        );
    }

    #[rstest]
    #[case(Some("evil.example.net"))]
    #[case(Some("example.com"))]
    #[case(None)]
    fn unknown_or_missing_hosts_are_not_found(#[case] host: Option<&str>) {
        assert_eq!(decide(host, "/"), HostRoute::NotFound);
    }

    #[rstest]
    #[case(Some("evil.example.net"))]
    #[case(Some("status.team.dev"))]
    #[case(None)]
    fn healthz_answers_on_any_host(#[case] host: Option<&str>) {
        assert_eq!(decide(host, "/healthz"), HostRoute::Pass);
    }

    #[rstest]
    #[case("status.example.com", "status.example.com")]
    #[case("Status.Example.COM:8080", "status.example.com")]
    #[case("status.example.com.", "status.example.com")]
    #[case("[::1]:8080", "::1")]
    #[case("[::1]", "::1")]
    #[case("127.0.0.1:3000", "127.0.0.1")]
    fn normalizes_hosts(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(normalize_host(input), expected);
    }

    #[test]
    fn domain_cache_lookups_are_case_insensitive_and_replaceable() {
        let cache = domains();
        assert_eq!(
            cache.slug_for("STATUS.team.dev"),
            Some("platform".to_owned())
        );
        cache.replace([]);
        assert_eq!(cache.slug_for("status.team.dev"), None);
    }
}
