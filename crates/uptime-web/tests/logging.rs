#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Request logs: one line per request, and handler errors with their cause.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use pretty_assertions::assert_eq;
use uptime_domain::{PageSpec, Theme};
use uptime_testkit::logs::Logs;

async fn app_with_page() -> TestApp {
    let app = TestApp::new().await;
    app.db
        .store()
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            sections: vec![],
        })
        .await
        .unwrap();
    app
}

#[tokio::test]
async fn every_request_gets_one_line_with_its_outcome() {
    let app = app_with_page().await;
    let (logs, _guard) = Logs::capture("info");
    let mut client = app.client();

    client.get("/s/platform").await;
    client.get("/s/nope").await;

    let ok = logs.with_message("GET /s/platform 200");
    assert_eq!(ok.len(), 1, "{:#?}", logs.lines());
    assert_eq!(ok[0]["level"], "INFO");
    assert_eq!(ok[0]["method"], "GET");
    assert_eq!(ok[0]["path"], "/s/platform");
    assert_eq!(ok[0]["status"], 200);
    assert_eq!(ok[0]["host"], common::APP_HOST);
    assert!(ok[0]["latency_ms"].is_u64());
    let missing = logs.with_message("GET /s/nope 404");
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0]["level"], "INFO", "a 404 is not our failure");
}

#[tokio::test]
async fn health_checks_stay_out_of_the_info_log() {
    let app = TestApp::new().await;
    let (logs, _guard) = Logs::capture("info");

    app.client().get("/healthz").await;

    assert!(logs.lines().is_empty(), "{:#?}", logs.lines());
}

#[tokio::test]
async fn push_tokens_never_reach_the_log() {
    let app = TestApp::new().await;
    let (logs, _guard) = Logs::capture("debug");

    app.client().get("/api/push/s3cr3t-token?status=up").await;

    let text = format!("{:?}", logs.lines());
    assert!(!text.contains("s3cr3t-token"), "{text}");
    assert_eq!(logs.with_message("GET /api/push/… 404").len(), 1, "{text}");
}

#[tokio::test]
async fn failing_requests_log_the_error_and_its_cause() {
    let app = app_with_page().await;
    let (client, connection) = tokio_postgres::connect(app.db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(r#"ALTER TABLE "status_pages" RENAME TO "gone""#)
        .await
        .unwrap();
    let (logs, _guard) = Logs::capture("info");

    let reply = app.client().get("/s/platform").await;

    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    let failed = logs.with_message("request failed");
    assert_eq!(failed.len(), 1, "{:#?}", logs.lines());
    assert_eq!(failed[0]["level"], "ERROR");
    assert_eq!(failed[0]["method"], "GET");
    assert_eq!(failed[0]["path"], "/s/platform");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains(r#"relation "status_pages" does not exist"#),
        "the root cause, not just the outer message: {error}"
    );
    let access = logs.with_message("GET /s/platform 500");
    assert_eq!(access.len(), 1);
    assert_eq!(access[0]["level"], "ERROR");
}
