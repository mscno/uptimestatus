#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! README badges for status pages and their components.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, ComponentStatus, Health, MonitorSpec, MonitorState,
    PageSpec, Runtime, SectionSpec, TcpCheck, Theme, Verdict,
};
use uptime_store::CheckRecord;

fn monitor(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: format!("{key} service"),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 443)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

/// Page `platform` shows monitor `api` and manual `cdn`; monitor `db` is on no page.
async fn setup(published: bool) -> (TestApp, i64) {
    let app = TestApp::new().await;
    let store = app.db.store();
    for key in ["api", "db"] {
        store
            .create_monitor(&monitor(key), Timestamp::now())
            .await
            .unwrap();
    }
    let page = store
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published,
            website: None,
            custom_css: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![
                    ComponentSpec::of_monitor("api".parse().unwrap(), Some("API".into())),
                    ComponentSpec::manual("cdn".parse().unwrap(), Some("CDN".into()), None),
                ],
            }],
        })
        .await
        .unwrap();
    (app, page.id)
}

async fn check_up(app: &TestApp, latency_ms: u64) {
    let store = app.db.store();
    let api = store
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "api")
        .unwrap();
    let now = Timestamp::now();
    store
        .record_check(&CheckRecord {
            monitor_id: api.id,
            scheduled_for: now,
            checked_at: now,
            verdict: Verdict {
                health: Health::Up,
                latency: Some(Duration::from_millis(latency_ms)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Up,
                consecutive_failures: 0,
            },
            transition: None,
            cert: None,
            next_run_at: now + jiff::SignedDuration::from_secs(60),
            region: "test".into(),
        })
        .await
        .unwrap();
    app.state.pages.clear();
}

/// The `<title>` of a badge: "label: message".
fn title(svg: &str) -> &str {
    svg.split("<title>")
        .nth(1)
        .and_then(|rest| rest.split("</title>").next())
        .unwrap_or_else(|| panic!("no title in {svg}"))
}

#[tokio::test]
async fn badges_show_page_and_component_status() {
    let (app, id) = setup(true).await;
    check_up(&app, 120).await;
    let mut public = app.client();

    let page = public.get("/s/platform/badge.svg").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers["content-type"], "image/svg+xml; charset=utf-8");
    assert_eq!(page.headers["cache-control"], "public, max-age=60");
    assert_eq!(title(&page.body), "status: operational");

    let api = public.get("/s/platform/badge/api/status.svg").await;
    assert_eq!(title(&api.body), "API: up");
    let cdn = public.get("/s/platform/badge/cdn/status.svg").await;
    assert_eq!(title(&cdn.body), "CDN: operational");

    let uptime = public
        .get("/s/platform/badge/api/uptime.svg?window=7d")
        .await;
    assert_eq!(title(&uptime.body), "uptime 7d: 100.00%");
    let response = public.get("/s/platform/badge/api/response.svg").await;
    assert_eq!(title(&response.body), "response time: 120 ms");

    // Options: style and a custom (escaped) label.
    let styled = public
        .get("/s/platform/badge/api/status.svg?style=for-the-badge&label=%3Cb%3EMy%20API")
        .await;
    assert_eq!(title(&styled.body), "&lt;B&gt;MY API: UP");
    assert!(!styled.body.contains("<b>"));

    // A manual status shows at once.
    app.db
        .store()
        .set_component_status(
            id,
            "cdn",
            Some(ComponentStatus::MajorOutage),
            None,
            "alice",
            Timestamp::now(),
        )
        .await
        .unwrap();
    app.state.pages.clear();
    let cdn = public.get("/s/platform/badge/cdn/status.svg").await;
    assert_eq!(title(&cdn.body), "CDN: major outage");
    let page = public.get("/s/platform/badge.svg").await;
    assert_eq!(title(&page.body), "status: major outage");
}

#[tokio::test]
async fn badges_reveal_nothing_beyond_the_page() {
    let (app, _) = setup(true).await;
    let mut public = app.client();
    for (path, status) in [
        ("/s/platform/badge/db/status.svg", StatusCode::NOT_FOUND),
        ("/s/platform/badge/nope/uptime.svg", StatusCode::NOT_FOUND),
        ("/s/nope/badge.svg", StatusCode::NOT_FOUND),
        // Manual components have no response time.
        ("/s/platform/badge/cdn/response.svg", StatusCode::NOT_FOUND),
        (
            "/s/platform/badge/api/uptime.svg?window=1y",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/s/platform/badge.svg?style=plastic",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(public.get(path).await.status, status, "{path}");
    }

    let (draft, _) = setup(false).await;
    let mut public = draft.client();
    assert_eq!(
        public.get("/s/platform/badge.svg").await.status,
        StatusCode::NOT_FOUND
    );
    let mut admin = draft.signed_in("alice", 1001).await;
    let seen = admin.get("/s/platform/badge.svg").await;
    assert_eq!(seen.status, StatusCode::OK);
    assert_eq!(seen.headers["cache-control"], "no-store");
}

#[tokio::test]
async fn badges_revalidate_with_etags() {
    let (app, _) = setup(true).await;
    let mut public = app.client();
    let first = public.get("/s/platform/badge/cdn/uptime.svg").await;
    let etag = first.headers["etag"].to_str().unwrap().to_owned();
    let again = public
        .get_with_header("/s/platform/badge/cdn/uptime.svg", "if-none-match", &etag)
        .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
}

#[tokio::test]
async fn the_console_offers_badge_snippets() {
    let (app, id) = setup(true).await;
    let mut admin = app.signed_in("alice", 1001).await;
    let editor = admin.get(&format!("/admin/pages/{id}")).await;
    editor
        .assert_contains("Badges")
        .assert_contains(r#"src="/s/platform/badge.svg""#)
        .assert_contains("[![Platform status](https://status.example.com/s/platform/badge.svg)](https://status.example.com/s/platform)")
        .assert_contains("badge/cdn/status.svg");

    let api = app
        .db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "api")
        .unwrap();
    let detail = admin.get(&format!("/admin/monitors/{}", api.id)).await;
    detail
        .assert_contains("Badges")
        .assert_contains("badge/api/response.svg");
    let db = app
        .db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "db")
        .unwrap();
    admin
        .get(&format!("/admin/monitors/{}", db.id))
        .await
        .assert_contains("Add this monitor to a status page to get README badges.");
}
