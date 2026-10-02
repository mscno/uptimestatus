#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Notification channels in the admin console, and choosing them per monitor.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{ChannelKind, ChannelSpec, CheckPolicy, CheckSpec, MonitorSpec, TcpCheck};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn webhook(name: &str, url: &str, default_on: bool) -> ChannelSpec {
    ChannelSpec {
        name: name.into(),
        kind: ChannelKind::Webhook,
        url: url.parse().unwrap(),
        secret: Some("s3cret".into()),
        default_on,
        routing: Default::default(),
    }
}

fn channel_form<'a>(name: &'a str, kind: &'a str, url: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![("name", name), ("kind", kind), ("url", url), ("secret", "")]
}

#[tokio::test]
async fn the_alerts_page_requires_signing_in() {
    let app = TestApp::new().await;
    assert_eq!(
        app.client().get("/admin/alerts").await.location(),
        Some("/login")
    );
}

#[tokio::test]
async fn channels_are_listed_without_their_secrets() {
    let app = TestApp::new().await;
    app.db
        .store()
        .create_channel(
            &ChannelSpec {
                name: "Ops Slack".into(),
                kind: ChannelKind::Slack,
                url: "https://hooks.slack.com/services/T0/B0/SECRETTOKEN"
                    .parse()
                    .unwrap(),
                secret: None,
                default_on: true,
                routing: Default::default(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();

    let page = app
        .signed_in("alice", 1001)
        .await
        .get("/admin/alerts")
        .await;

    page.assert_contains("Ops Slack")
        .assert_contains("hooks.slack.com")
        .assert_contains("Recent deliveries");
    assert!(
        !page.body.contains("SECRETTOKEN"),
        "webhook URLs are secrets"
    );
}

#[tokio::test]
async fn admins_create_edit_and_delete_channels() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let created = admin
        .post_form(
            "/admin/alerts",
            &channel_form("Ops", "slack", "https://hooks.slack.com/services/T/B/x"),
        )
        .await;
    assert_eq!(
        created.location(),
        Some("/admin/alerts"),
        "{}",
        created.body
    );
    let channel = app.db.store().list_channels().await.unwrap().remove(0);
    assert_eq!(channel.spec.kind, ChannelKind::Slack);

    let edit_url = format!("/admin/alerts/{}", channel.id);
    admin
        .get(&edit_url)
        .await
        .assert_contains("https://hooks.slack.com/services/T/B/x");
    let mut fields = channel_form(
        "Ops team",
        "slack",
        "https://hooks.slack.com/services/T/B/y",
    );
    fields.push(("default_on", "on"));
    let updated = admin.post_form(&edit_url, &fields).await;
    assert_eq!(updated.location(), Some("/admin/alerts"));
    let channel = app.db.store().channel(channel.id).await.unwrap().unwrap();
    assert_eq!(channel.spec.name, "Ops team");
    assert!(channel.spec.default_on);

    let deleted = admin.post_form(&format!("{edit_url}/delete"), &[]).await;
    assert_eq!(deleted.location(), Some("/admin/alerts"));
    assert!(app.db.store().list_channels().await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_channels_are_explained() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let reply = admin
        .post_form(
            "/admin/alerts",
            &channel_form("Ops", "slack", "https://example.com/hook"),
        )
        .await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply.assert_contains("does not look like a Slack webhook URL");
    let blank = admin
        .post_form("/admin/alerts", &channel_form("", "webhook", "not a url"))
        .await;
    blank
        .assert_contains("Give the channel a name.")
        .assert_contains("Enter a full URL");
}

#[tokio::test]
async fn a_blank_secret_keeps_the_current_one() {
    let app = TestApp::new().await;
    let channel = app
        .db
        .store()
        .create_channel(
            &webhook("Hook", "https://ops.example.com/h", false),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;
    let edit_url = format!("/admin/alerts/{}", channel.id);

    let form = admin.get(&edit_url).await;
    assert!(!form.body.contains("s3cret"), "secrets are never shown");
    admin
        .post_form(
            &edit_url,
            &channel_form("Hook", "webhook", "https://ops.example.com/h2"),
        )
        .await;

    let channel = app.db.store().channel(channel.id).await.unwrap().unwrap();
    assert_eq!(channel.spec.secret.as_deref(), Some("s3cret"));
    assert_eq!(channel.spec.url.as_str(), "https://ops.example.com/h2");
}

#[tokio::test]
async fn send_test_reports_the_status_but_not_the_body() {
    let app = TestApp::new().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&receiver)
        .await;
    Mock::given(method("POST"))
        .and(path("/broken"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&receiver)
        .await;
    let store = app.db.store();
    let ok = store
        .create_channel(
            &webhook("Ok", &format!("{}/ok", receiver.uri()), false),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let broken = store
        .create_channel(
            &webhook("Broken", &format!("{}/broken", receiver.uri()), false),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;

    admin
        .datastar_form(&format!("/admin/alerts/{}/test", ok.id), &[])
        .await
        .assert_contains("datastar-patch-elements")
        .assert_contains(&format!("test-{}", ok.id))
        .assert_contains("Delivered");
    // The status is reported; the receiver's body is not (messages are stored,
    // and a response can echo secrets back).
    let failed = admin
        .datastar_form(&format!("/admin/alerts/{}/test", broken.id), &[])
        .await;
    failed
        .assert_contains("Failed")
        .assert_contains("answered 500");
    assert!(!failed.body.contains("boom"), "{}", failed.body);
}

fn tcp(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: key.into(),
        check: CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

#[tokio::test]
async fn monitors_choose_channels_with_defaults_preselected() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let default = store
        .create_channel(
            &webhook("Default", "https://a.example.com/h", true),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let other = store
        .create_channel(
            &webhook("Other", "https://b.example.com/h", false),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;

    let new_form = admin.get("/admin/monitors/new").await;
    new_form
        .assert_contains(&format!(r#"name="channel_{}""#, default.id))
        .assert_contains(&format!(r#"name="channel_{}""#, other.id));
    let default_toggle = &new_form.body[new_form
        .body
        .find(&format!(r#"name="channel_{}""#, default.id))
        .unwrap()..];
    assert!(
        default_toggle[..default_toggle.find('>').unwrap()].contains(r#"checked="true""#),
        "default channels start on"
    );

    let monitor = store
        .create_monitor(&tcp("db"), Timestamp::now())
        .await
        .unwrap();
    let on = format!("channel_{}", other.id);
    let fields = [
        ("key", "db"),
        ("name", "db"),
        ("check_type", "tcp"),
        ("host", "db.example.com"),
        ("port", "5432"),
        ("interval", "1m"),
        ("retry_interval", "20s"),
        ("timeout", "10s"),
        ("retries", "1"),
        ("resend_every", "0"),
        ("active", "on"),
        (on.as_str(), "on"),
    ];
    let saved = admin
        .post_form(&format!("/admin/monitors/{}", monitor.id), &fields)
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert_eq!(
        store.monitor_channels(monitor.id).await.unwrap(),
        [other.id]
    );

    let edit = admin
        .get(&format!("/admin/monitors/{}/edit", monitor.id))
        .await;
    let other_toggle = &edit.body[edit
        .body
        .find(&format!(r#"name="channel_{}""#, other.id))
        .unwrap()..];
    assert!(other_toggle[..other_toggle.find('>').unwrap()].contains(r#"checked="true""#));
}

#[tokio::test]
async fn channels_have_routing_that_round_trips_through_the_form() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let mut fields = channel_form("Pager", "slack", "https://hooks.slack.com/services/T/B/x");
    fields.extend([
        ("escalate_after", "15"),
        ("quiet_start", "22:30"),
        ("quiet_end", "06:00"),
        ("mute_recovered", "on"),
        ("mute_cert", "on"),
    ]);

    let created = admin.post_form("/admin/alerts", &fields).await;

    assert_eq!(
        created.location(),
        Some("/admin/alerts"),
        "{}",
        created.body
    );
    let channel = app.db.store().list_channels().await.unwrap().remove(0);
    let routing = &channel.spec.routing;
    assert_eq!(routing.escalate_after_mins, 15);
    assert_eq!(
        routing.quiet,
        Some(uptime_domain::QuietHours {
            start_min: 22 * 60 + 30,
            end_min: 6 * 60
        })
    );
    assert_eq!(
        routing.mute,
        [
            uptime_domain::AlertEvent::Recovered,
            uptime_domain::AlertEvent::CertExpiring
        ]
    );
    admin
        .get(&format!("/admin/alerts/{}", channel.id))
        .await
        .assert_contains(r#"value="22:30""#)
        .assert_contains(r#"value="15""#);
    admin
        .get("/admin/alerts")
        .await
        .assert_contains("after 15 min · quiet 22:30–06:00 · mutes recovered, certificates");

    let mut bad = channel_form("Pager", "slack", "https://hooks.slack.com/services/T/B/x");
    bad.extend([
        ("escalate_after", "soon"),
        ("quiet_start", "25:00"),
        ("quiet_end", "06:00"),
    ]);
    let reply = admin.post_form("/admin/alerts", &bad).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply
        .assert_contains("whole number of minutes")
        .assert_contains("HH:MM");
}
