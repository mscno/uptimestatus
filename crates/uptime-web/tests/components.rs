#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Manual components, status overrides and custom CSS on status pages.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, MonitorSpec, PageSpec, SectionSpec, TcpCheck, Theme,
    TokenScope,
};
use uptime_store::{AuditQuery, NewApiToken};

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

/// Page `platform`: monitor `api` and manual component `cdn`.
async fn setup() -> (TestApp, i64) {
    let app = TestApp::new().await;
    let store = app.db.store();
    store
        .create_monitor(&monitor("api"), Timestamp::now())
        .await
        .unwrap();
    let page = store
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
                    ComponentSpec::of_monitor("api".parse().unwrap(), Some("API".into())),
                    ComponentSpec::manual(
                        "cdn".parse().unwrap(),
                        Some("CDN".into()),
                        Some("Static assets worldwide".into()),
                    ),
                ],
            }],
        })
        .await
        .unwrap();
    (app, page.id)
}

async fn token(app: &TestApp, scope: TokenScope) -> String {
    let secret = uptime_web::new_api_token();
    app.db
        .store()
        .create_api_token(
            &NewApiToken {
                name: "deploys".into(),
                token_hash: uptime_web::hash_api_token(&secret),
                scope,
                created_by: "alice".into(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    secret
}

fn json_of(reply: &common::Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

#[tokio::test]
async fn manual_components_show_their_status_and_raise_the_headline() {
    let (app, id) = setup().await;
    let mut public = app.client();
    let page = public.get("/s/platform").await;
    page.assert_contains("CDN")
        .assert_contains("Static assets worldwide")
        .assert_contains(r#"data-component="cdn""#)
        .assert_contains(r#"data-status="operational""#);

    let mut admin = app.signed_in("alice", 1001).await;
    let editor = admin.get(&format!("/admin/pages/{id}")).await;
    editor
        .assert_contains("Component status")
        .assert_contains(&format!("/admin/pages/{id}/components/cdn/status"));

    // One click sets a status, with a note; the page and its live stream follow.
    let stream = public.open_stream("/s/platform/live").await;
    let set = admin
        .post_form(
            &format!("/admin/pages/{id}/components/cdn/status"),
            &[
                ("status", "major_outage"),
                ("note", "Edge provider down"),
                ("back", &format!("/admin/pages/{id}")),
            ],
        )
        .await;
    assert_eq!(set.location(), Some(format!("/admin/pages/{id}").as_str()));
    common::read_stream_until(stream, "Major outage").await;
    let page = public.get("/s/platform").await;
    page.assert_contains("Major outage")
        .assert_contains(r#"data-status="major_outage""#);

    let summary = json_of(&public.get("/s/platform/summary.json").await);
    assert_eq!(summary["status"], "major_outage");
    let cdn = &summary["components"][1];
    assert_eq!(
        (&cdn["key"], &cdn["manual_status"], &cdn["status"]),
        (&json!("cdn"), &json!("major_outage"), &json!("down"))
    );

    // The history names who did it and why, and the change is audited.
    admin
        .get(&format!("/admin/pages/{id}/components"))
        .await
        .assert_contains("Edge provider down")
        .assert_contains("alice");
    let events = app
        .db
        .store()
        .audit_events(&AuditQuery {
            action: Some("component".into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor, "alice");
    assert_eq!(
        events[0].details["changed"]["status"],
        json!({ "from": "operational", "to": "major_outage" })
    );

    // Manual components cannot be cleared; unknown statuses are refused.
    let clear = admin
        .post_form(
            &format!("/admin/pages/{id}/components/cdn/status"),
            &[("status", "clear")],
        )
        .await;
    assert_eq!(clear.status, StatusCode::NOT_FOUND);
    let bogus = admin
        .post_form(
            &format!("/admin/pages/{id}/components/cdn/status"),
            &[("status", "on_fire")],
        )
        .await;
    assert_eq!(bogus.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn overrides_replace_a_monitors_status_until_cleared() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let url = format!("/admin/pages/{id}/components/api/status");
    admin
        .post_form(&url, &[("status", "degraded"), ("note", "Slow writes")])
        .await;
    let mut public = app.client();
    let page = public.get("/s/platform").await;
    page.assert_contains("Degraded performance")
        .assert_contains(r#"data-component="api" data-status="degraded""#);
    admin
        .get(&format!("/admin/pages/{id}/components"))
        .await
        .assert_contains("Override: ")
        .assert_contains("Clear override");

    admin.post_form(&url, &[("status", "clear")]).await;
    let page = public.get("/s/platform").await;
    assert!(!page.body.contains("Degraded performance"));
    let summary = json_of(&public.get("/s/platform/summary.json").await);
    assert_eq!(summary["components"][0]["manual_status"], Value::Null);
    assert_eq!(
        app.db.store().component_changes(id).await.unwrap()["api"].len(),
        2
    );
}

#[tokio::test]
async fn group_components_can_be_overridden_and_keep_their_members() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let mut member = monitor("api");
    member.group = Some("prod".into());
    store
        .create_monitor(&member, Timestamp::now())
        .await
        .unwrap();
    let group = MonitorSpec {
        key: "platform".parse().unwrap(),
        name: "Platform".into(),
        check: CheckSpec::Group(uptime_domain::GroupCheck {
            path: "prod".into(),
            mode: uptime_domain::GroupMode::Any,
            show_members: true,
        }),
        ..monitor("platform")
    };
    store
        .create_monitor(&group, Timestamp::now())
        .await
        .unwrap();
    let page = store
        .create_page(&PageSpec {
            slug: "status".parse().unwrap(),
            title: "Status".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            custom_css: None,
            sections: vec![SectionSpec {
                name: "All".into(),
                components: vec![ComponentSpec::of_monitor("platform".parse().unwrap(), None)],
            }],
        })
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_form(
            &format!("/admin/pages/{}/components/platform/status", page.id),
            &[("status", "maintenance"), ("note", "Planned migration")],
        )
        .await;
    let mut public = app.client();
    let shown = public.get("/s/status").await;
    shown
        .assert_contains(r#"data-component="platform" data-status="maintenance""#)
        .assert_contains("Scheduled maintenance")
        .assert_contains("api service");
}

#[tokio::test]
async fn the_api_lists_and_sets_component_statuses() {
    let (app, _) = setup().await;
    let read = token(&app, TokenScope::Read).await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();

    let listed = client
        .api(
            "GET",
            "/api/v1/pages/platform/components",
            Some(&read),
            None,
        )
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    let components = &json_of(&listed)["components"];
    assert_eq!(components[0]["key"], "api");
    assert_eq!(components[0]["kind"], "monitor");
    assert_eq!(components[0]["status"], Value::Null);
    assert_eq!(components[1]["kind"], "manual");
    assert_eq!(components[1]["status"], "operational");

    let url = "/api/v1/pages/platform/components/cdn/status";
    let denied = client
        .api(
            "PUT",
            url,
            Some(&read),
            Some(json!({ "status": "degraded" })),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let set = client
        .api(
            "PUT",
            url,
            Some(&write),
            Some(json!({ "status": "partial_outage", "note": "EU only" })),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let body = json_of(&set);
    assert_eq!(
        (&body["status"], &body["actor"], &body["note"]),
        (
            &json!("partial_outage"),
            &json!("api:deploys"),
            &json!("EU only")
        )
    );
    client
        .get("/s/platform")
        .await
        .assert_contains("Partial outage");

    for (path, body, status) in [
        (
            url,
            json!({ "status": null }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            url,
            json!({ "status": "on_fire" }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/pages/platform/components/nope/status",
            json!({ "status": "degraded" }),
            StatusCode::NOT_FOUND,
        ),
        (
            "/api/v1/pages/nope/components/cdn/status",
            json!({ "status": "degraded" }),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let reply = client.api("PUT", path, Some(&write), Some(body)).await;
        assert_eq!(reply.status, status, "{path}: {}", reply.body);
    }
    let events = app
        .db
        .store()
        .audit_events(&AuditQuery {
            actor: Some("api:deploys".into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(events[0].action, "component.status");
}

#[tokio::test]
async fn custom_css_is_served_as_its_own_stylesheet() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let mut public = app.client();
    assert_eq!(
        public.get("/s/platform/custom.css").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!public.get("/s/platform").await.body.contains("custom.css"));

    let fields = [
        ("slug", "platform"),
        ("title", "Platform"),
        ("theme", "auto"),
        ("look", "pixel"),
        ("published", "on"),
        (
            "layout",
            "## Core\napi | API\nmanual:cdn | CDN | Static assets worldwide\n",
        ),
        ("custom_css", ".status-header { border-color: hotpink }"),
    ];
    let saved = admin
        .post_form(&format!("/admin/pages/{id}"), &fields)
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);

    let page = public.get("/s/platform").await;
    let link = page
        .body
        .split("href=\"")
        .find(|part| part.starts_with("/s/platform/custom.css?v="))
        .expect("the stylesheet is linked")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let css = public.get(&link).await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.headers["content-type"], "text/css; charset=utf-8");
    assert_eq!(css.headers["x-content-type-options"], "nosniff");
    assert_eq!(css.body, ".status-header { border-color: hotpink }");
    // Revalidation answers 304.
    let etag = css.headers["etag"].to_str().unwrap().to_owned();
    let revalidated = public
        .get_with_header("/s/platform/custom.css", "if-none-match", &etag)
        .await;
    assert_eq!(revalidated.status, StatusCode::NOT_MODIFIED);
    // It works on the custom-domain shape of URLs too (they map to /s/{slug}).
    assert!(link.contains(&etag.trim_matches('"')[..8]));

    // Unsafe CSS is refused with a message, and nothing changes.
    let mut bad = fields;
    bad[6] = ("custom_css", "a { background: url(javascript:alert(1)) }");
    let refused = admin.post_form(&format!("/admin/pages/{id}"), &bad).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    refused.assert_contains("`javascript:` URLs are not allowed.");
    assert_eq!(
        public.get("/s/platform/custom.css").await.body,
        ".status-header { border-color: hotpink }"
    );

    // The audit trail records the change by length, not the stylesheet.
    let events = app
        .db
        .store()
        .audit_events(&AuditQuery {
            action: Some("page.update".into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        events[0].details["changed"]["custom_css"],
        json!({ "from": { "length": 0 }, "to": { "length": 40 } })
    );
    assert!(!events[0].details.to_string().contains("hotpink"));
}

#[tokio::test]
async fn pages_with_manual_components_round_trip_through_the_api() {
    let (app, _) = setup().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();
    let page = json_of(
        &client
            .api("GET", "/api/v1/pages/platform", Some(&write), None)
            .await,
    );
    assert_eq!(
        page["section"][0]["components"][1],
        json!({ "manual": "cdn", "label": "CDN", "description": "Static assets worldwide" })
    );
    let mut changed = page.clone();
    changed["custom_css"] = json!("@import url(http://insecure.example/x.css);");
    let refused = client
        .api("PUT", "/api/v1/pages/platform", Some(&write), Some(changed))
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("https"), "{}", refused.body);
    let clash = json!({
        "title": "Platform",
        "section": [{ "name": "Core", "components": [{ "monitor": "api" }, { "manual": "api" }] }]
    });
    let refused = client
        .api("PUT", "/api/v1/pages/platform", Some(&write), Some(clash))
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
}
