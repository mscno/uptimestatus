#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! End to end: real listeners, a real database, real HTTP.

use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uptime_testkit::TestDb;
use uptime_web::{AppState, Hosts};

#[tokio::test]
async fn serves_public_and_internal_listeners_until_shutdown() {
    let db = TestDb::new().await;
    let state = AppState::new(
        db.store().clone(),
        Hosts::new(
            "https://status.example.com".parse().unwrap(),
            "edge.example.com",
        ),
    );
    let public = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let internal = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (public_addr, internal_addr) =
        (public.local_addr().unwrap(), internal.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(uptime_server::serve::serve(
        state,
        public,
        internal,
        shutdown.clone(),
    ));

    let client = reqwest::Client::new();
    let home = client
        .get(format!("http://{public_addr}/"))
        .header("host", "status.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(home.status(), 200);
    assert!(home.text().await.unwrap().contains("uptimestatus"));

    let unknown_host = client
        .get(format!("http://{public_addr}/"))
        .header("host", "nope.example")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_host.status(), 404);

    let ready = client
        .get(format!("http://{internal_addr}/readyz"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);

    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server stops promptly");
    result.unwrap().unwrap();
}

#[tokio::test]
async fn migrate_command_is_idempotent_against_a_fresh_database() {
    let db = TestDb::new().await;
    let report = db.store().migrate().await.unwrap();
    assert_eq!(
        report.applied(),
        0,
        "template databases arrive fully migrated"
    );
}

#[tokio::test]
async fn verified_domains_are_loaded_for_routing_and_tls() {
    let db = TestDb::new().await;
    let store = db.store();
    let page = store
        .create_page(&uptime_domain::PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: uptime_domain::Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            sections: vec![],
        })
        .await
        .unwrap();
    let now = jiff::Timestamp::now();
    let verified = store
        .add_domain(page.id, &"status.team.dev".parse().unwrap(), now)
        .await
        .unwrap();
    store
        .record_domain_check(verified.id, Ok(()), now)
        .await
        .unwrap();
    store
        .add_domain(page.id, &"pending.team.dev".parse().unwrap(), now)
        .await
        .unwrap();

    let domains = uptime_server::serve::load_domains(store).await.unwrap();

    assert_eq!(
        domains.slug_for("status.team.dev").as_deref(),
        Some("platform")
    );
    assert_eq!(domains.slug_for("pending.team.dev"), None);
}
