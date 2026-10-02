//! HTTP surface of uptimestatus.
//!
//! Two routers, served on two listeners:
//! - [`public_router`] (`PORT`): everything browsers reach through the TLS-terminating proxy,
//!   routed by hostname first (see [`host`]), then by Topcoat.
//! - [`internal_router`] (`INTERNAL_PORT`): health and readiness, reachable
//!   only over the private network.

pub mod admin;
mod api;
mod api_v1;
mod assets;
pub mod auth;
mod chart;
pub mod fmt;
pub mod host;
mod internal;
mod logging;
pub mod media;
mod metrics;
mod pages;
mod state;
mod status;
mod views;

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse as _, Response},
    routing::get,
};
use topcoat::{
    cookie::RouterBuilderCookieExt as _,
    router::tower::TowerService,
    session::{RouterBuilderSessionExt as _, SessionConfig},
};
use tower::Layer as _;

pub use api_v1::{hash_token as hash_api_token, new_token as new_api_token};
pub use host::{DomainCache, HostRoute, Hosts};
pub use state::AppState;
pub use status::PageCache;

/// The public application: host routing in front of Topcoat pages.
pub fn public_router(state: AppState) -> Router {
    let media = state.media.clone();
    let origin_state = state.clone();
    let routes = Router::new()
        .route("/healthz", get(internal::healthz))
        .route("/static/{*path}", get(assets::serve))
        .route(
            "/media/{name}",
            get(move |name| media::serve(media.clone(), name)),
        )
        .fallback_service(TowerService::new(topcoat_router(&state)));
    // Wrapping the router (instead of `Router::layer`) runs host routing
    // *before* route matching, so rewritten paths are routed correctly.
    let host_routed =
        axum::middleware::from_fn_with_state(state, host::route_by_host).layer(routes);
    Router::new()
        .fallback_service(host_routed)
        .layer(axum::middleware::from_fn_with_state(
            origin_state,
            same_origin_admin_mutations,
        ))
        .layer(axum::middleware::from_fn(no_store_sensitive))
        .layer(axum::middleware::from_fn(logging::log_request))
}

/// SameSite=Lax cookies are still sent by browsers from a different origin
/// on the same site. Reject those origins before any cookie-authenticated
/// admin action reaches its handler.
async fn same_origin_admin_mutations(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method();
    let path = request.uri().path();
    let admin = path == "/admin"
        || path
            .strip_prefix("/admin")
            .is_some_and(|rest| rest.starts_with('/'));
    let cookie_mutation = admin || path == "/auth/logout";
    let unsafe_method = !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    if cookie_mutation && unsafe_method {
        let expected = state.hosts.app_url().origin().ascii_serialization();
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .map(|value| value.to_str().ok());
        let wrong_origin = origin.is_some_and(|value| value != Some(expected.as_str()));
        let wrong_site = request
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|site| site != "same-origin" && site != "none");
        if wrong_origin || wrong_site {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

/// Console and API responses may carry credentials, including on error paths.
async fn no_store_sensitive(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let sensitive = ["/admin", "/auth", "/api/v1", "/api/push"]
        .iter()
        .any(|prefix| {
            path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        });
    let mut response = next.run(request).await;
    if sensitive {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// The internal application (health, readiness).
pub fn internal_router(state: AppState) -> Router {
    internal::router(state).layer(axum::middleware::from_fn(logging::log_internal_request))
}

fn topcoat_router(state: &AppState) -> topcoat::router::Router {
    topcoat::router::Router::builder()
        .cookies()
        .sessions(
            SessionConfig::builder()
                .lifetime(state.auth.session_idle)
                .build(),
        )
        .app_context(state.clone())
        .app_context(state.auth.cookie_key.clone())
        .layer(logging::LogErrors)
        // Public
        .route(pages::root)
        .page(status::status_page)
        .page(status::maintenance_page)
        .page(status::incidents_page)
        .page(status::incident_page)
        .route(status::status_body_patch)
        .route(status::summary_json)
        .route(status::status_feed)
        .route(status::status_live)
        .route(api::push)
        .route(api_v1::list_monitors)
        .route(api_v1::get_monitor)
        .route(api_v1::put_monitor)
        .route(api_v1::delete_monitor)
        .route(api_v1::pause_monitor)
        .route(api_v1::resume_monitor)
        .route(api_v1::monitor_checks)
        .route(api_v1::get_config)
        .route(api_v1::get_page)
        .route(api_v1::put_page)
        .route(api_v1::delete_page)
        .route(api_v1::create_incident)
        .route(api_v1::post_incident_update)
        // Auth
        .page(auth::login_page)
        .route(auth::github_start)
        .route(auth::github_callback)
        .route(auth::logout)
        .route(auth::dev_login)
        // Admin console
        .page(admin::dashboard)
        .route(admin::live)
        .route(admin::bulk_action)
        .route(admin::api_page)
        .route(admin::create_token)
        .route(admin::revoke_token)
        .page(admin::new_monitor)
        .route(admin::create_monitor)
        .route(admin::test_unsaved)
        .page(admin::monitor_detail)
        .page(admin::edit_monitor)
        .route(admin::update_monitor)
        .route(admin::test_saved)
        .route(admin::monitor_live)
        .route(admin::pause_monitor)
        .route(admin::resume_monitor)
        .route(admin::delete_monitor)
        .page(admin::pages_list)
        .page(admin::new_page)
        .route(admin::create_page)
        .page(admin::edit_page)
        .route(admin::update_page)
        .route(admin::delete_page)
        .route(admin::export_toml)
        .route(admin::upload_image)
        .route(admin::remove_image)
        .route(admin::add_domain)
        .route(admin::verify_domain)
        .route(admin::remove_domain)
        .page(admin::alerts_page)
        .page(admin::new_channel)
        .route(admin::create_channel)
        .page(admin::edit_channel)
        .route(admin::update_channel)
        .route(admin::delete_channel)
        .route(admin::test_channel)
        .page(admin::incidents_page)
        .page(admin::new_incident)
        .route(admin::create_incident)
        .page(admin::incident_detail)
        .route(admin::post_incident_update)
        .route(admin::delete_incident)
        .page(admin::maintenance_page)
        .page(admin::new_maintenance)
        .route(admin::create_maintenance)
        .page(admin::edit_maintenance)
        .route(admin::update_maintenance)
        .route(admin::delete_maintenance)
        .build()
}
