#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Push monitor heartbeats and the monitor form's push and DNS types.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{CheckPolicy, CheckSpec, DnsRecordType, MonitorSpec, PushCheck};

const TOKEN: &str = "abcdefghijklmnopqrstuvwx";

async fn push_monitor(app: &TestApp) -> uptime_store::Monitor {
    app.db
        .store()
        .create_monitor(
            &MonitorSpec {
                key: "nightly-backup".parse().unwrap(),
                name: "Nightly backup".into(),
                check: CheckSpec::Push(PushCheck {
                    token: TOKEN.into(),
                }),
                policy: CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            Timestamp::now() + jiff::SignedDuration::from_hours(1),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn pushes_are_recorded_and_make_the_monitor_due() {
    let app = TestApp::new().await;
    let monitor = push_monitor(&app).await;

    let reply = app
        .client()
        .get(&format!("/api/push/{TOKEN}?status=up&msg=OK&ping=12.5"))
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, r#"{"ok":true}"#);
    let runtime = app.db.store().runtime(monitor.id).await.unwrap().unwrap();
    assert!(runtime.next_run_at <= Timestamp::now(), "judged at once");
    let claim = app
        .db
        .store()
        .claim_due(Timestamp::now(), 10, "test")
        .await
        .unwrap()
        .remove(0);
    let push = claim.last_push.unwrap();
    assert!(push.up);
    assert_eq!(push.message.as_deref(), Some("OK"));
    assert_eq!(
        push.ping,
        Some(std::time::Duration::from_millis(12)),
        "stored in whole ms"
    );
}

#[tokio::test]
async fn services_can_report_themselves_down_by_post() {
    let app = TestApp::new().await;
    push_monitor(&app).await;

    let reply = app
        .client()
        .post_form(
            &format!("/api/push/{TOKEN}?status=down&msg=disk%20full"),
            &[],
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    let claim = app
        .db
        .store()
        .claim_due(Timestamp::now(), 10, "test")
        .await
        .unwrap()
        .remove(0);
    let push = claim.last_push.unwrap();
    assert!(!push.up);
    assert_eq!(push.message.as_deref(), Some("disk full"));
}

#[tokio::test]
async fn unknown_tokens_are_not_found() {
    let app = TestApp::new().await;
    let reply = app.client().get("/api/push/nope").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("unknown push token"));
}

fn form<'a>(
    check_type: &'a str,
    extra: &[(&'static str, &'a str)],
) -> Vec<(&'static str, &'a str)> {
    let mut fields = vec![
        ("key", "thing"),
        ("name", "Thing"),
        ("check_type", check_type),
        ("interval", "5m"),
        ("retry_interval", "1m"),
        ("timeout", "10s"),
        ("retries", "0"),
        ("resend_every", "0"),
        ("active", "on"),
    ];
    fields.extend_from_slice(extra);
    fields
}

#[tokio::test]
async fn push_monitors_get_a_token_and_show_their_url() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let created = admin.post_form("/admin/monitors", &form("push", &[])).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);

    let monitor = app.db.store().list_monitors().await.unwrap().remove(0);
    let CheckSpec::Push(push) = &monitor.spec.check else {
        panic!("{:?}", monitor.spec.check)
    };
    assert_eq!(push.token.len(), 24);
    admin
        .get(created.location().unwrap())
        .await
        .assert_contains(&format!(
            "https://status.example.com/api/push/{}",
            push.token
        ))
        .assert_contains("sb-copy-button");

    // Saving again keeps the token.
    admin
        .post_form(
            &format!("/admin/monitors/{}", monitor.id),
            &form("push", &[("push_token", push.token.as_str())]),
        )
        .await;
    let again = app.db.store().monitor(monitor.id).await.unwrap().unwrap();
    assert_eq!(again.spec.check, monitor.spec.check);
}

#[tokio::test]
async fn dns_monitors_are_created_from_the_form() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let created = admin
        .post_form(
            "/admin/monitors",
            &form(
                "dns",
                &[
                    ("dns_name", "example.com"),
                    ("record_type", "TXT"),
                    ("resolver", "1.1.1.1"),
                    ("expect", "v=spf1"),
                ],
            ),
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let monitor = app.db.store().list_monitors().await.unwrap().remove(0);
    let CheckSpec::Dns(dns) = monitor.spec.check else {
        panic!()
    };
    assert_eq!(dns.record_type, DnsRecordType::Txt);
    assert_eq!(dns.expect.as_deref(), Some("v=spf1"));

    let bad = admin
        .post_form(
            "/admin/monitors",
            &form(
                "dns",
                &[("dns_name", "not a name"), ("resolver", "one.one")],
            ),
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    bad.assert_contains("Enter a domain name")
        .assert_contains("Enter an IP address");
}
