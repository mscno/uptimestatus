#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt as _;
use pretty_assertions::assert_eq;
use tower::ServiceExt as _;
use uptime_testkit::TestDb;
use uptime_web::{AppState, DomainCache, Hosts};

const APP: &str = "status.example.com";
const CUSTOM: &str = "status.team.dev";

fn state(db: &TestDb) -> AppState {
    let domains = DomainCache::default();
    domains.replace([(CUSTOM.to_owned(), "platform".to_owned())]);
    AppState::new(
        db.store().clone(),
        Hosts::new(
            format!("https://{APP}").parse().unwrap(),
            "edge.example.com",
        ),
    )
    .with_domains(domains)
}

async fn get(app: Router, host: &str, path: &str) -> (StatusCode, Option<String>, String) {
    let request = Request::get(path)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[tokio::test]
async fn app_host_root_leads_to_the_console() {
    let db = TestDb::new().await;
    let (status, location, _) = get(uptime_web::public_router(state(&db)), APP, "/").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin"));
}

/// A published, empty status page `platform`.
async fn platform_page(db: &TestDb) {
    let spec = uptime_domain::PageSpec {
        slug: "platform".parse().unwrap(),
        title: "Platform Status".into(),
        description: None,
        accent: None,
        theme: uptime_domain::Theme::Auto,
        look: Default::default(),
        published: true,
        website: None,
        sections: vec![],
    };
    db.store().create_page(&spec).await.unwrap();
}

#[tokio::test]
async fn status_pages_are_reachable_on_the_app_host() {
    let db = TestDb::new().await;
    platform_page(&db).await;
    let (status, _, body) = get(uptime_web::public_router(state(&db)), APP, "/s/platform").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>Platform Status</h1>"), "{body}");
    assert!(body.starts_with("<!DOCTYPE html>"), "{body}");
}

#[tokio::test]
async fn custom_domain_root_serves_its_status_page() {
    let db = TestDb::new().await;
    platform_page(&db).await;
    let (status, _, body) = get(uptime_web::public_router(state(&db)), CUSTOM, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>Platform Status</h1>"), "{body}");
}

#[tokio::test]
async fn custom_domain_cannot_reach_other_routes() {
    let db = TestDb::new().await;
    let app = uptime_web::public_router(state(&db));
    // `/` on the app host is the home page; on a custom domain it must not be.
    let (status, _, body) = get(app.clone(), CUSTOM, "/s/other").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _, _) = get(app, CUSTOM, "/admin").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_hosts_get_404() {
    let db = TestDb::new().await;
    let (status, _, _) = get(
        uptime_web::public_router(state(&db)),
        "evil.example.net",
        "/",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn edge_host_redirects_to_the_app() {
    let db = TestDb::new().await;
    let (status, location, _) = get(
        uptime_web::public_router(state(&db)),
        "edge.example.com",
        "/s/x",
    )
    .await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(location.as_deref(), Some("https://status.example.com/s/x"));
}

#[tokio::test]
async fn public_healthz_answers_any_host() {
    let db = TestDb::new().await;
    let (status, _, body) = get(
        uptime_web::public_router(state(&db)),
        "10.0.0.7:8080",
        "/healthz",
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
}

#[tokio::test]
async fn internal_readyz_checks_the_database() {
    let db = TestDb::new().await;
    let (status, _, body) = get(
        uptime_web::internal_router(state(&db)),
        "app.internal",
        "/readyz",
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ready"));
}
