#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Editing and duplicating monitors and status pages from the lists.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use uptime_domain::{
    ChannelKind, ChannelSpec, CheckPolicy, CheckSpec, ComponentSpec, HttpCheck, MonitorSpec,
    PageSpec, PushCheck, SectionSpec, Theme,
};

fn http(key: &str, name: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: name.into(),
        check: CheckSpec::Http(HttpCheck::get(
            "https://api.example.com/health".parse().unwrap(),
        )),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

fn page() -> PageSpec {
    PageSpec {
        slug: "platform".parse().unwrap(),
        title: "Platform Status".into(),
        description: Some("Core services".into()),
        accent: None,
        theme: Theme::Dark,
        look: Default::default(),
        published: true,
        website: None,
        sections: vec![SectionSpec {
            name: "Core".into(),
            components: vec![ComponentSpec {
                monitor: "api".parse().unwrap(),
                label: Some("Public API".into()),
            }],
        }],
    }
}

#[tokio::test]
async fn the_monitor_list_links_to_edit_and_duplicate() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();

    let list = app.signed_in("alice", 1001).await.get("/admin").await;

    list.assert_contains(&format!(r#"href="/admin/monitors/{}/edit""#, monitor.id))
        .assert_contains(&format!(
            r#"href="/admin/monitors/new?from={}""#,
            monitor.id
        ));
}

#[tokio::test]
async fn duplicating_a_monitor_prefills_a_copy_with_its_channels() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let monitor = store
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();
    let channel = store
        .create_channel(
            &ChannelSpec {
                name: "Ops".into(),
                kind: ChannelKind::Webhook,
                url: "https://ops.example.com/hook".parse().unwrap(),
                secret: None,
                default_on: false,
                routing: Default::default(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[channel.id])
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;

    let form = admin
        .get(&format!("/admin/monitors/new?from={}", monitor.id))
        .await;

    assert_eq!(form.status, StatusCode::OK);
    form.assert_contains(r#"value="api-copy""#)
        .assert_contains(r#"value="API (copy)""#)
        .assert_contains(r#"value="https://api.example.com/health""#)
        .assert_contains(r#"action="/admin/monitors""#);
    let toggle = &form.body[form
        .body
        .find(&format!(r#"name="channel_{}""#, channel.id))
        .unwrap()..];
    assert!(toggle[..toggle.find('>').unwrap()].contains(r#"checked="true""#));
}

#[tokio::test]
async fn copies_get_the_next_free_key() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let monitor = store
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();
    store
        .create_monitor(&http("api-copy", "API (copy)"), Timestamp::now())
        .await
        .unwrap();

    app.signed_in("alice", 1001)
        .await
        .get(&format!("/admin/monitors/new?from={}", monitor.id))
        .await
        .assert_contains(r#"value="api-copy-2""#);
}

#[tokio::test]
async fn a_duplicated_push_monitor_gets_its_own_token() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(
            &MonitorSpec {
                check: CheckSpec::Push(PushCheck {
                    token: "sharedtokensharedtoken12".into(),
                }),
                ..http("cron", "Cron")
            },
            Timestamp::now(),
        )
        .await
        .unwrap();

    let form = app
        .signed_in("alice", 1001)
        .await
        .get(&format!("/admin/monitors/new?from={}", monitor.id))
        .await;

    assert!(
        !form.body.contains("sharedtokensharedtoken12"),
        "a new token is made on save"
    );
}

#[tokio::test]
async fn duplicating_an_unknown_monitor_is_not_found() {
    let app = TestApp::new().await;
    let reply = app
        .signed_in("alice", 1001)
        .await
        .get("/admin/monitors/new?from=4242")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn monitor_and_page_screens_offer_duplicate() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let monitor = store
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();
    let page = store.create_page(&page()).await.unwrap();
    let mut admin = app.signed_in("alice", 1001).await;

    admin
        .get(&format!("/admin/monitors/{}", monitor.id))
        .await
        .assert_contains(&format!(
            r#"href="/admin/monitors/new?from={}""#,
            monitor.id
        ));
    admin
        .get(&format!("/admin/pages/{}", page.id))
        .await
        .assert_contains(&format!(r#"href="/admin/pages/new?from={}""#, page.id));
}

#[tokio::test]
async fn the_page_list_links_to_edit_and_duplicate() {
    let app = TestApp::new().await;
    let store = app.db.store();
    store
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();
    let page = store.create_page(&page()).await.unwrap();

    app.signed_in("alice", 1001)
        .await
        .get("/admin/pages")
        .await
        .assert_contains(&format!(r#"href="/admin/pages/{}""#, page.id))
        .assert_contains(&format!(r#"href="/admin/pages/new?from={}""#, page.id));
}

#[tokio::test]
async fn duplicating_a_page_prefills_a_draft_copy() {
    let app = TestApp::new().await;
    let store = app.db.store();
    store
        .create_monitor(&http("api", "API"), Timestamp::now())
        .await
        .unwrap();
    let page = store.create_page(&page()).await.unwrap();

    let form = app
        .signed_in("alice", 1001)
        .await
        .get(&format!("/admin/pages/new?from={}", page.id))
        .await;

    assert_eq!(form.status, StatusCode::OK);
    form.assert_contains(r#"value="platform-copy""#)
        .assert_contains(r#"value="Platform Status (copy)""#)
        .assert_contains("api | Public API")
        .assert_contains(r#"action="/admin/pages""#);
    let published = &form.body[form.body.find(r#"name="published""#).unwrap()..];
    assert!(
        published[..published.find('>').unwrap()].contains(r#"checked="false""#),
        "copies start as drafts"
    );
}

#[tokio::test]
async fn the_degraded_threshold_explains_its_format() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    admin
        .get("/admin/monitors/new")
        .await
        .assert_contains("800ms")
        .assert_contains("1.5s")
        .assert_contains("below the timeout");

    let reply = admin
        .post_form(
            "/admin/monitors",
            &[
                ("key", "api"),
                ("name", "API"),
                ("check_type", "http"),
                ("url", "https://api.example.com/"),
                ("interval", "1m"),
                ("timeout", "10s"),
                ("retries", "0"),
                ("resend_every", "0"),
                ("degraded_after", "fast"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply.assert_contains("like 800ms, 1.5s or 2s");
}
