#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Incident templates and scheduled incident updates in the console and API.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use serde_json::json;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, Impact, IncidentStatus, IncidentTemplate, MonitorSpec,
    PageSpec, SectionSpec, TcpCheck, Theme, TokenScope,
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

/// Monitor `api` on the published page `platform`.
async fn setup() -> TestApp {
    let app = TestApp::new().await;
    let store = app.db.store();
    store
        .create_monitor(&monitor("api"), Timestamp::now())
        .await
        .unwrap();
    store
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform Status".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            custom_css: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![ComponentSpec::of_monitor("api".parse().unwrap(), None)],
            }],
        })
        .await
        .unwrap();
    app
}

fn slow() -> IncidentTemplate {
    IncidentTemplate {
        name: "Slow".into(),
        title: "{monitor} is slow".into(),
        impact: Impact::Minor,
        status: IncidentStatus::Identified,
        body: "Requests to {monitors} on {page} are slow since {started_at}.".into(),
    }
}

/// A `datetime-local` value (UTC) `minutes` from now.
fn local_in(minutes: i64) -> String {
    (Timestamp::now() + SignedDuration::from_mins(minutes))
        .strftime("%Y-%m-%dT%H:%M")
        .to_string()
}

async fn audit(app: &TestApp, action: &str) -> Vec<uptime_store::AuditEvent> {
    app.db
        .store()
        .audit_events(&AuditQuery {
            action: Some(action.into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn admins_manage_templates() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .get("/admin/incidents/templates")
        .await
        .assert_contains("No templates");
    admin
        .get("/admin/incidents/templates/new")
        .await
        .assert_contains("{monitor}");

    let created = admin
        .post_form(
            "/admin/incidents/templates",
            &[
                ("name", "Slow"),
                ("title", "{monitor} is slow"),
                ("impact", "minor"),
                ("status", "identified"),
                ("body", "Requests are slow."),
            ],
        )
        .await;
    assert_eq!(created.location(), Some("/admin/incidents/templates"));
    let template = app
        .db
        .store()
        .template_by_name("Slow")
        .await
        .unwrap()
        .unwrap();
    admin
        .get("/admin/incidents/templates")
        .await
        .assert_contains("Slow")
        .assert_contains("{monitor} is slow");

    // Mistakes are explained and nothing is saved.
    let invalid = admin
        .post_form(
            "/admin/incidents/templates",
            &[
                ("name", "Slow"),
                ("title", "x"),
                ("impact", "minor"),
                ("status", "identified"),
                ("body", "x"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    invalid.assert_contains("already exists");

    let edit = format!("/admin/incidents/templates/{}", template.id);
    admin
        .get(&format!("{edit}/edit"))
        .await
        .assert_contains("Requests are slow.");
    let updated = admin
        .post_form(
            &edit,
            &[
                ("name", "Very slow"),
                ("title", "{monitor} is very slow"),
                ("impact", "major"),
                ("status", "investigating"),
                ("body", "Very slow."),
            ],
        )
        .await;
    assert_eq!(updated.location(), Some("/admin/incidents/templates"));
    let stored = app.db.store().template(template.id).await.unwrap().unwrap();
    assert_eq!(stored.spec.impact, Impact::Major);

    let deleted = admin.post_form(&format!("{edit}/delete"), &[]).await;
    assert_eq!(deleted.location(), Some("/admin/incidents/templates"));
    assert!(app.db.store().list_templates().await.unwrap().is_empty());
    let actions: Vec<String> = audit(&app, "template")
        .await
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert_eq!(
        actions,
        ["template.delete", "template.update", "template.create"]
    );
}

#[tokio::test]
async fn templates_prefill_forms_and_placeholders_expand_on_publish() {
    let app = setup().await;
    let template = app.db.store().create_template(&slow()).await.unwrap();
    let mut admin = app.signed_in("alice", 1001).await;

    let form = admin
        .get(&format!("/admin/incidents/new?template={}", template.id))
        .await;
    form.assert_contains(r#"value="{monitor} is slow""#)
        .assert_contains("Requests to {monitors} on {page} are slow")
        .assert_contains(r#"<option value="identified" selected"#);

    let declared = admin
        .post_form(
            "/admin/incidents",
            &[
                ("title", "{monitor} is slow"),
                ("impact", "minor"),
                ("status", "identified"),
                ("message", "Requests to {monitors} on {page} are slow."),
                ("monitor_api", "on"),
            ],
        )
        .await;
    let incident = app.db.store().list_incidents(10).await.unwrap().remove(0);
    assert_eq!(
        declared.location(),
        Some(format!("/admin/incidents/{}", incident.id).as_str())
    );
    assert_eq!(incident.title, "api service is slow");
    assert_eq!(
        incident.updates[0].body,
        "Requests to api service on Platform Status are slow."
    );

    // The update form takes the template's message and status.
    admin
        .get(&format!(
            "/admin/incidents/{}?template={}",
            incident.id, template.id
        ))
        .await
        .assert_contains("Requests to {monitors} on {page} are slow since {started_at}.");

    // The API accepts a template by name, filling what is not given.
    let secret = uptime_web::new_api_token();
    app.db
        .store()
        .create_api_token(
            &NewApiToken {
                name: "ci".into(),
                token_hash: uptime_web::hash_api_token(&secret),
                scope: TokenScope::Write,
                created_by: "alice".into(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut client = app.client();
    let reply = client
        .api(
            "POST",
            "/api/v1/incidents",
            Some(&secret),
            Some(json!({ "template": "Slow", "monitors": ["api"] })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply
        .assert_contains("api service is slow")
        .assert_contains(r#""status":"identified""#);
    let created: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let update = client
        .api(
            "POST",
            &format!("/api/v1/incidents/{}/updates", created["id"]),
            Some(&secret),
            Some(json!({ "template": "Slow" })),
        )
        .await;
    assert_eq!(update.status, StatusCode::OK, "{}", update.body);
    let updated = app
        .db
        .store()
        .incident(created["id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        updated.updates[0]
            .body
            .starts_with("Requests to api service on Platform Status are slow since "),
        "{}",
        updated.updates[0].body
    );
    let unknown = client
        .api(
            "POST",
            "/api/v1/incidents",
            Some(&secret),
            Some(json!({ "template": "Nope", "monitors": ["api"] })),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn scheduled_declarations_and_updates_publish_later() {
    let app = setup().await;
    let store = app.db.store();
    let mut admin = app.signed_in("alice", 1001).await;
    let mut visitor = app.client();

    // A declaration scheduled for later is not public yet.
    let scheduled = admin
        .post_form(
            "/admin/incidents",
            &[
                ("title", "Planned database upgrade"),
                ("impact", "minor"),
                ("status", "identified"),
                ("message", "We are upgrading {monitors}."),
                ("monitor_api", "on"),
                ("publish_at", &local_in(120)),
            ],
        )
        .await;
    assert_eq!(scheduled.location(), Some("/admin/incidents"));
    assert!(store.list_incidents(10).await.unwrap().is_empty());
    let list = admin.get("/admin/incidents").await;
    list.assert_contains("Scheduled")
        .assert_contains("Planned database upgrade");
    assert!(
        !visitor
            .get("/s/platform")
            .await
            .body
            .contains("Planned database upgrade")
    );

    // Editing moves it; cancelling a copy removes it.
    let declaration = store.scheduled_declarations().await.unwrap().remove(0);
    let edit = format!("/admin/incidents/scheduled/{}", declaration.id);
    admin
        .get(&edit)
        .await
        .assert_contains("Planned database upgrade");
    let moved = admin
        .post_form(
            &edit,
            &[
                ("title", "Planned database upgrade"),
                ("impact", "major"),
                ("status", "identified"),
                ("message", "We are upgrading {monitors} now."),
                ("publish_at", &local_in(60)),
            ],
        )
        .await;
    assert_eq!(moved.location(), Some("/admin/incidents"));
    let declaration = store.scheduled(declaration.id).await.unwrap().unwrap();
    assert_eq!(declaration.body, "We are upgrading {monitors} now.");

    // The worker publishes it once due, as the admin who scheduled it.
    let later = Timestamp::from_second(Timestamp::now().as_second() + 61 * 60).unwrap();
    assert_eq!(
        uptime_web::publish::publish_due(&app.state, Timestamp::now())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        uptime_web::publish::publish_due(&app.state, later)
            .await
            .unwrap(),
        1
    );
    let incident = store.list_incidents(10).await.unwrap().remove(0);
    assert_eq!(incident.impact, Impact::Major);
    assert_eq!(
        incident.updates[0].body,
        "We are upgrading api service now."
    );
    assert_eq!(incident.started_at, later);
    let declared = audit(&app, "incident.declare").await.remove(0);
    assert_eq!(declared.actor, "alice");
    assert!(
        declared.summary.ends_with("(scheduled)"),
        "{}",
        declared.summary
    );
    visitor
        .get("/s/platform")
        .await
        .assert_contains("Planned database upgrade");

    // A scheduled resolution shows on the incident until it publishes.
    let detail = format!("/admin/incidents/{}", incident.id);
    let resolution = admin
        .post_form(
            &format!("{detail}/updates"),
            &[
                ("status", "resolved"),
                ("message", "The upgrade is complete."),
                ("publish_at", &local_in(180)),
            ],
        )
        .await;
    assert_eq!(resolution.location(), Some(detail.as_str()));
    admin
        .get(&detail)
        .await
        .assert_contains("Scheduled for")
        .assert_contains("The upgrade is complete.");
    assert_eq!(
        store.incident(incident.id).await.unwrap().unwrap().status,
        IncidentStatus::Identified
    );
    let pending = store.scheduled_for_incident(incident.id).await.unwrap();
    let cancelled = admin
        .post_form(
            &format!("/admin/incidents/scheduled/{}/cancel", pending[0].id),
            &[],
        )
        .await;
    assert_eq!(cancelled.location(), Some(detail.as_str()));
    assert!(
        store
            .scheduled_for_incident(incident.id)
            .await
            .unwrap()
            .is_empty()
    );
    let actions: Vec<String> = audit(&app, "incident")
        .await
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert!(
        actions.contains(&"incident.schedule".to_owned()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"incident.reschedule".to_owned()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"incident.unschedule".to_owned()),
        "{actions:?}"
    );
}

#[tokio::test]
async fn failed_schedules_wait_with_their_error() {
    let app = setup().await;
    let store = app.db.store();
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_form(
            "/admin/incidents",
            &[
                ("title", "Later"),
                ("impact", "minor"),
                ("status", "investigating"),
                ("message", "Later."),
                ("monitor_api", "on"),
                ("publish_at", &local_in(5)),
            ],
        )
        .await;
    // Its monitor is deleted before it comes due.
    let api = store.list_monitors().await.unwrap().remove(0);
    store.delete_monitor(api.id).await.unwrap();
    let later = Timestamp::now() + SignedDuration::from_mins(10);
    assert_eq!(
        uptime_web::publish::publish_due(&app.state, later)
            .await
            .unwrap(),
        0
    );
    let failed = store.scheduled_declarations().await.unwrap().remove(0);
    assert!(failed.error.unwrap().contains("api"));
    admin
        .get("/admin/incidents")
        .await
        .assert_contains("Could not publish");
    assert_eq!(
        audit(&app, "incident.schedule_failed").await[0].actor,
        "system"
    );
}
