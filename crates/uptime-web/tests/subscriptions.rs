#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Email in the console and on status pages: email channels, page senders,
//! subscribing by email or webhook, managing and leaving, the subscriber
//! list, and Lettermint's webhook. Without email configured, none of it shows.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use serde_json::json;
use tower::ServiceExt as _;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, MonitorSpec, PageSpec, SectionSpec, SubscriberStatus,
    TcpCheck, Theme,
};
use uptime_notify::{Mailer, MailerConfig};
use uptime_store::{LinkPurpose, Subscriber};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn monitor(key: &str, name: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: name.into(),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 443)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

/// Monitors `api` and `db` on the published page `platform`.
async fn with_page(app: &TestApp) -> i64 {
    let store = &app.state.store;
    store
        .create_monitor(&monitor("api", "API service"), Timestamp::now())
        .await
        .unwrap();
    store
        .create_monitor(&monitor("db", "Database"), Timestamp::now())
        .await
        .unwrap();
    store
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            custom_css: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![
                    ComponentSpec::of_monitor("api".parse().unwrap(), Some("Public API".into())),
                    ComponentSpec::of_monitor("db".parse().unwrap(), None),
                ],
            }],
        })
        .await
        .unwrap()
        .id
}

/// A test app with email configured against a mock Lettermint.
async fn with_email(lettermint: &MockServer, team: bool) -> TestApp {
    let mut app = TestApp::new().await;
    let mailer = Mailer::new(MailerConfig {
        project_token: "lm_project_test".into(),
        team_token: team.then(|| "lm_team_test".to_owned()),
        default_from: "uptimestatus <alerts@parap.ly>".parse().unwrap(),
        api_url: Some(lettermint.uri()),
    })
    .unwrap();
    app.state.store = app.db.store().clone().with_subscriptions(true);
    app.state.sender = Some(app.state.sender.take().unwrap().with_mailer(mailer));
    app.state = app
        .state
        .clone()
        .with_lettermint_webhook_secret(Some("whsec_test".into()));
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({ "message_id": "m", "status": "pending" })),
        )
        .mount(lettermint)
        .await;
    app
}

async fn only_subscriber(app: &TestApp, page: i64) -> Subscriber {
    let mut list = app.state.store.list_subscribers(page).await.unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    list.remove(0)
}

async fn token(app: &TestApp, id: i64, purpose: LinkPurpose) -> String {
    app.state
        .store
        .subscriber_token(
            id,
            purpose,
            Timestamp::now() + jiff::SignedDuration::from_hours(1),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn without_email_nothing_offers_or_queues_email() {
    let app = TestApp::new().await;
    let page = with_page(&app).await;
    let mut visitor = app.client();
    let status = visitor.get("/s/platform").await;
    assert_eq!(status.status, StatusCode::OK);
    assert!(!status.body.contains("Subscribe to updates"));
    assert_eq!(
        visitor.get("/s/platform/subscribe").await.status,
        StatusCode::NOT_FOUND
    );
    let posted = visitor
        .post_form("/s/platform/subscribe", &[("email", "reader@example.com")])
        .await;
    assert_eq!(posted.status, StatusCode::NOT_FOUND);
    assert!(
        app.state
            .store
            .list_subscribers(page)
            .await
            .unwrap()
            .is_empty()
    );

    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .get(&format!("/admin/pages/{page}/email"))
        .await
        .assert_contains("Email is not configured");
    admin
        .get("/admin/alerts/new")
        .await
        .assert_contains("Email (not configured)");
    let refused = admin
        .post_form(
            "/admin/alerts",
            &[
                ("name", "On call"),
                ("kind", "email"),
                ("recipients", "ops@example.com"),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    refused.assert_contains("Email is not configured");
    assert!(app.state.store.list_channels().await.unwrap().is_empty());
}

#[tokio::test]
async fn visitors_subscribe_by_email_confirm_manage_and_leave() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, false).await;
    let page = with_page(&app).await;
    let mut visitor = app.client();
    visitor
        .get("/s/platform")
        .await
        .assert_contains("/s/platform/subscribe")
        .assert_contains("Subscribe to updates");
    visitor
        .get("/s/platform/subscribe")
        .await
        .assert_contains("Get updates by email")
        .assert_contains("Public API")
        .assert_contains("Database");

    let bad = visitor
        .post_form("/s/platform/subscribe", &[("email", "not an address")])
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    bad.assert_contains("Enter a valid email address.");

    // A bot that fills in the hidden field is told the same, but nothing happens.
    visitor
        .post_form(
            "/s/platform/subscribe",
            &[("email", "bot@example.com"), ("website", "http://spam")],
        )
        .await
        .assert_contains("Check your inbox");
    assert!(
        app.state
            .store
            .list_subscribers(page)
            .await
            .unwrap()
            .is_empty()
    );

    let done = visitor
        .post_form(
            "/s/platform/subscribe",
            &[("email", "reader@example.com"), ("scope", "all")],
        )
        .await;
    assert_eq!(done.status, StatusCode::OK);
    done.assert_contains("Check your inbox");
    // Asking again says the same (never reveals an existing subscription).
    visitor
        .post_form("/s/platform/subscribe", &[("email", "Reader@example.com")])
        .await
        .assert_contains("Check your inbox");
    let reader = only_subscriber(&app, page).await;
    assert_eq!(reader.status, SubscriberStatus::Pending);

    // The emailed link shows a button; scanners fetching it confirm nothing.
    let confirm = token(&app, reader.id, LinkPurpose::Confirm).await;
    visitor
        .get(&format!(
            "/s/platform/subscriptions/confirm?token={confirm}"
        ))
        .await
        .assert_contains("Confirm subscription");
    assert_eq!(
        only_subscriber(&app, page).await.status,
        SubscriberStatus::Pending
    );
    let confirmed = visitor
        .post_form("/s/platform/subscriptions/confirm", &[("token", &confirm)])
        .await;
    let manage = confirmed.location().unwrap().to_owned();
    assert!(
        manage.starts_with("/s/platform/subscriptions/manage?token="),
        "{manage}"
    );
    assert_eq!(
        only_subscriber(&app, page).await.status,
        SubscriberStatus::Active
    );
    visitor
        .get(&manage)
        .await
        .assert_contains("Subscription confirmed.")
        .assert_contains("Re***@example.com");

    // Follow only the database.
    let manage_token = manage
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let saved = visitor
        .post_form(
            "/s/platform/subscriptions/manage",
            &[
                ("token", &manage_token),
                ("scope", "some"),
                ("component_db", "on"),
            ],
        )
        .await;
    assert!(saved.location().unwrap().contains("saved=1"));
    assert_eq!(only_subscriber(&app, page).await.selection.0, ["db"]);

    // One-click unsubscribe (RFC 8058): a POST to the emailed link.
    let leave = token(&app, reader.id, LinkPurpose::Unsubscribe).await;
    let left = visitor
        .post_form(
            &format!("/s/platform/subscriptions/unsubscribe?token={leave}"),
            &[("List-Unsubscribe", "One-Click")],
        )
        .await;
    assert_eq!(left.status, StatusCode::OK);
    left.assert_contains("You are unsubscribed");
    assert_eq!(
        only_subscriber(&app, page).await.status,
        SubscriberStatus::Unsubscribed
    );
    // Old links stop working.
    let stale = visitor.get(&manage).await;
    assert_eq!(stale.status, StatusCode::NOT_FOUND);
    stale.assert_contains("This link has expired");
}

#[tokio::test]
async fn webhook_subscriptions_need_a_public_https_url_and_show_their_secret_once() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, false).await;
    let page = with_page(&app).await;
    let mut visitor = app.client();
    // (Private addresses are refused too in production; this harness allows them.)
    for url in [
        "http://hooks.example.com/x",
        "https://localhost/x",
        "not a url",
    ] {
        let refused = visitor
            .post_form(
                "/s/platform/subscribe/webhook",
                &[("url", url), ("email", "owner@example.com")],
            )
            .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{url}");
        refused.assert_contains("public https:// URL");
    }
    visitor
        .post_form(
            "/s/platform/subscribe/webhook",
            &[
                ("url", "https://hooks.example.com/status"),
                ("email", "owner@example.com"),
            ],
        )
        .await
        .assert_contains("To finish adding the webhook");
    let hook = only_subscriber(&app, page).await;
    assert_eq!(
        hook.webhook_url.as_ref().unwrap().as_str(),
        "https://hooks.example.com/status"
    );
    let confirm = token(&app, hook.id, LinkPurpose::Confirm).await;
    let manage = visitor
        .post_form("/s/platform/subscriptions/confirm", &[("token", &confirm)])
        .await
        .location()
        .unwrap()
        .to_owned();
    let first = visitor.get(&manage).await;
    first
        .assert_contains("Signing secret")
        .assert_contains("whsec_")
        .assert_contains("Webhook to hooks.example.com");
    let again = visitor.get(&manage).await;
    assert!(!again.body.contains("whsec_"), "shown once");
}

#[tokio::test]
async fn admins_configure_the_sender_and_review_subscribers() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, false).await;
    let page = with_page(&app).await;
    let mut admin = app.signed_in("alice", 1001).await;
    let saved = admin
        .post_form(
            &format!("/admin/pages/{page}/email"),
            &[
                ("subscriptions", "on"),
                ("sender_name", "Platform"),
                ("sender_address", "status@mail.example.com"),
                ("reply_to", "support@example.com"),
            ],
        )
        .await;
    assert!(saved.location().unwrap().contains("notice=Saved"));
    let settings = app.state.store.email_settings(page).await.unwrap();
    assert!(settings.subscriptions && !settings.notify_automatic);
    assert_eq!(
        settings.sender_address.unwrap().as_str(),
        "status@mail.example.com"
    );
    admin
        .get(&format!("/admin/pages/{page}/email"))
        .await
        .assert_contains("Lettermint dashboard")
        .assert_contains("Send a test email to");

    // Without the Team API, a test accepted from the page's own sender
    // marks it verified.
    let tested = admin
        .post_form(
            &format!("/admin/pages/{page}/email/test"),
            &[("to", "alice@example.com")],
        )
        .await;
    assert!(
        tested.location().unwrap().contains("now+sends+from+it"),
        "{:?}",
        tested.location()
    );
    let sent: serde_json::Value =
        serde_json::from_slice(&lettermint.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["from"], "\"Platform\" <status@mail.example.com>");
    assert_eq!(sent["to"], json!(["alice@example.com"]));
    assert!(
        app.state
            .store
            .email_settings(page)
            .await
            .unwrap()
            .verified_sender()
            .is_some()
    );

    // Two subscribers, listed masked; export and removal are audited.
    let mut visitor = app.client();
    for email in ["one@example.com", "two@example.com"] {
        visitor
            .post_form("/s/platform/subscribe", &[("email", email)])
            .await;
    }
    let list = admin.get(&format!("/admin/pages/{page}/subscribers")).await;
    list.assert_contains("on***@example.com")
        .assert_contains("tw***@example.com")
        .assert_contains("Awaiting confirmation");
    assert!(!list.body.contains("one@example.com"));
    let csv = admin
        .get(&format!("/admin/pages/{page}/subscribers.csv"))
        .await;
    assert_eq!(csv.headers["content-type"], "text/csv; charset=utf-8");
    assert!(csv.body.contains("\"one@example.com\""), "{}", csv.body);
    let one = app
        .state
        .store
        .list_subscribers(page)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.email.as_str() == "one@example.com")
        .unwrap();
    admin
        .post_form(
            &format!("/admin/pages/{page}/subscribers/{}/remove", one.id),
            &[],
        )
        .await;
    assert_eq!(
        app.state.store.list_subscribers(page).await.unwrap().len(),
        1
    );
    let actions: Vec<String> = app
        .state
        .store
        .audit_events(&uptime_store::AuditQuery {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.action)
        .collect();
    for action in [
        "page.email",
        "page.email_test",
        "subscriber.export",
        "subscriber.remove",
    ] {
        assert!(
            actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }
}

#[tokio::test]
async fn sending_domains_are_set_up_through_the_team_api() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, true).await;
    let page = with_page(&app).await;
    let domain = json!({
        "id": "dom_1", "domain": "mail.example.com", "dkim_mode": "cname",
        "rotation_ready": true, "status_changed_at": null, "created_at": "2026-10-15T10:00:00Z",
        "dns_records": [{
            "id": "r1", "type": "CNAME", "hostname": "lm1._domainkey",
            "fqdn": "lm1._domainkey.mail.example.com", "content": "lm1.dkim.lmta.net",
            "status": "pending", "purpose": "dkim_primary", "verification_scope": "required",
            "required_for_verification": true, "verified_at": null, "last_checked_at": null
        }]
    });
    Mock::given(method("POST"))
        .and(path("/domains"))
        .respond_with(ResponseTemplate::new(201).set_body_json(domain.clone()))
        .mount(&lettermint)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains/dom_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(domain))
        .mount(&lettermint)
        .await;
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_form(
            &format!("/admin/pages/{page}/email"),
            &[
                ("subscriptions", "on"),
                ("sender_address", "status@mail.example.com"),
            ],
        )
        .await;
    admin
        .get(&format!("/admin/pages/{page}/email"))
        .await
        .assert_contains("Set up sending domain");
    let set_up = admin
        .post_form(&format!("/admin/pages/{page}/email/domain"), &[])
        .await;
    assert!(
        set_up.location().unwrap().contains("notice="),
        "{:?}",
        set_up.location()
    );
    admin
        .get(&format!("/admin/pages/{page}/email"))
        .await
        .assert_contains("lm1._domainkey.mail.example.com")
        .assert_contains("lm1.dkim.lmta.net")
        .assert_contains("Waiting for DNS")
        .assert_contains("Verify");
}

#[tokio::test]
async fn email_channels_send_alerts_and_tests() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, false).await;
    let mut admin = app.signed_in("alice", 1001).await;
    let invalid = admin
        .post_form(
            "/admin/alerts",
            &[
                ("name", "On call"),
                ("kind", "email"),
                ("recipients", "ops@example.com\nnope"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    invalid.assert_contains("is not a valid email address");
    let created = admin
        .post_form(
            "/admin/alerts",
            &[
                ("name", "On call"),
                ("kind", "email"),
                ("recipients", "ops@example.com\noncall@example.com"),
            ],
        )
        .await;
    assert_eq!(created.location(), Some("/admin/alerts"));
    let channel = app.state.store.list_channels().await.unwrap().remove(0);
    let list = admin.get("/admin/alerts").await;
    list.assert_contains("op***@example.com, on***@example.com");
    admin
        .get(&format!("/admin/alerts/{}", channel.id))
        .await
        .assert_contains("oncall@example.com");
    let tested = admin
        .datastar_form(&format!("/admin/alerts/{}/test", channel.id), &[])
        .await;
    assert_eq!(tested.status, StatusCode::OK);
    let sent: serde_json::Value =
        serde_json::from_slice(&lettermint.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["to"], json!(["ops@example.com", "oncall@example.com"]));
    assert_eq!(sent["from"], "\"uptimestatus\" <alerts@parap.ly>");
}

async fn lettermint_hook(app: &TestApp, body: &str, signature: Option<String>) -> StatusCode {
    let now = Timestamp::now().as_second();
    let mut request = Request::post("/webhooks/lettermint")
        .header("host", common::APP_HOST)
        .header("content-type", "application/json")
        .header("x-lettermint-delivery", now.to_string());
    if let Some(signature) = signature {
        request = request.header("x-lettermint-signature", signature);
    }
    uptime_web::public_router(app.state.clone())
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
        .status()
}

fn sign(body: &str) -> String {
    let now = Timestamp::now().as_second();
    let v1 = uptime_notify::signature("whsec_test", now, body.as_bytes());
    format!("t={now},v1={}", v1.trim_start_matches("sha256="))
}

#[tokio::test]
async fn lettermint_bounces_suppress_subscribers() {
    let lettermint = MockServer::start().await;
    let app = with_email(&lettermint, false).await;
    let page = with_page(&app).await;
    app.client()
        .post_form("/s/platform/subscribe", &[("email", "gone@example.com")])
        .await;
    let body = r#"{"id":"d1","event":"message.hard_bounced","timestamp":"2026-10-15T10:00:00Z","data":{"recipient":"gone@example.com"}}"#;
    assert_eq!(
        lettermint_hook(&app, body, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        lettermint_hook(&app, body, Some(sign("{}"))).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        lettermint_hook(&app, body, Some(sign(body))).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        only_subscriber(&app, page).await.status,
        SubscriberStatus::Suppressed
    );
    let events = app
        .state
        .store
        .audit_events(&uptime_store::AuditQuery {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    let suppressed = events
        .iter()
        .find(|e| e.action == "subscriber.suppress")
        .unwrap();
    assert_eq!(suppressed.actor, "system");
    assert!(
        !suppressed.summary.contains("gone@"),
        "{}",
        suppressed.summary
    );

    // Without a configured secret the endpoint does not exist.
    let mut plain = TestApp::new().await;
    plain.state = plain.state.clone().with_lettermint_webhook_secret(None);
    assert_eq!(
        lettermint_hook(&plain, body, Some(sign(body))).await,
        StatusCode::NOT_FOUND
    );
}
