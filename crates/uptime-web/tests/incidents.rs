#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Incidents and maintenance windows: the admin console and the status pages.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, Impact, IncidentStatus, MaintenanceSpec, MonitorSpec,
    PageSpec, SectionSpec, TcpCheck, Theme,
};
use uptime_store::NewIncident;

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

/// Monitors `api` and `db`; page `platform` shows only `api`.
async fn setup() -> TestApp {
    let app = TestApp::new().await;
    let store = app.db.store();
    store
        .create_monitor(&monitor("api"), Timestamp::now())
        .await
        .unwrap();
    store
        .create_monitor(&monitor("db"), Timestamp::now())
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
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![ComponentSpec {
                    monitor: "api".parse().unwrap(),
                    label: None,
                }],
            }],
        })
        .await
        .unwrap();
    app
}

fn hours(h: i64) -> SignedDuration {
    SignedDuration::from_hours(h)
}

// ── Status pages ─────────────────────────────────────────────────────────

#[tokio::test]
async fn open_incidents_and_their_updates_are_shown() {
    let app = setup().await;
    let store = app.db.store();
    let incident = store
        .declare_incident(
            &NewIncident {
                title: "Elevated API errors".into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                message: "We are looking into it.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    store
        .post_incident_update(
            incident.id,
            IncidentStatus::Identified,
            "A bad deploy; rolling back.",
            Timestamp::now(),
        )
        .await
        .unwrap();
    store
        .declare_incident(
            &NewIncident {
                title: "Database hiccup".into(),
                impact: Impact::Minor,
                status: IncidentStatus::Investigating,
                message: "Slow queries.".into(),
                monitors: vec!["db".parse().unwrap()],
            },
            Timestamp::now(),
        )
        .await
        .unwrap();

    let page = app.client().get("/s/platform").await;

    page.assert_contains("Partial outage")
        .assert_contains("Elevated API errors")
        .assert_contains("Identified")
        .assert_contains("A bad deploy; rolling back.")
        .assert_contains("We are looking into it.");
    assert!(
        !page.body.contains("Database hiccup"),
        "not this page's monitors"
    );

    let json: serde_json::Value =
        serde_json::from_str(&app.client().get("/s/platform/summary.json").await.body).unwrap();
    assert_eq!(json["incidents"][0]["title"], "Elevated API errors");
    assert_eq!(json["incidents"][0]["status"], "identified");
}

#[tokio::test]
async fn scheduled_and_active_maintenance_is_announced() {
    let app = setup().await;
    let store = app.db.store();
    let now = Timestamp::now();
    for (title, starts, ends, key) in [
        ("Load balancer swap", now - hours(1), now + hours(1), "api"),
        ("Kernel upgrade", now + hours(24), now + hours(26), "api"),
        ("Database failover", now + hours(2), now + hours(3), "db"),
        (
            "Next month",
            now + hours(24 * 30),
            now + hours(24 * 30 + 1),
            "api",
        ),
    ] {
        store
            .create_maintenance(
                &MaintenanceSpec {
                    title: title.into(),
                    description: None,
                    starts_at: starts,
                    ends_at: ends,
                    repeat: None,
                    repeat_until: None,
                    monitors: vec![key.parse().unwrap()],
                },
                now,
            )
            .await
            .unwrap();
    }

    let page = app.client().get("/s/platform").await;

    page.assert_contains("Load balancer swap")
        .assert_contains("In progress")
        .assert_contains("Kernel upgrade")
        .assert_contains("Scheduled");
    assert!(!page.body.contains("Database failover"));
    assert!(!page.body.contains("Next month"), "only the coming week");
}

// ── Admin: incidents ─────────────────────────────────────────────────────

#[tokio::test]
async fn admins_declare_update_and_resolve_incidents() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .get("/admin/incidents/new")
        .await
        .assert_contains(r#"name="monitor_api""#);

    let declared = admin
        .post_form(
            "/admin/incidents",
            &[
                ("title", "API outage"),
                ("impact", "critical"),
                ("status", "investigating"),
                ("message", "Requests are failing."),
                ("monitor_api", "on"),
            ],
        )
        .await;
    assert_eq!(declared.status, StatusCode::SEE_OTHER, "{}", declared.body);
    let url = declared.location().unwrap().to_owned();
    let incident = app.db.store().list_incidents(10).await.unwrap().remove(0);
    assert_eq!(url, format!("/admin/incidents/{}", incident.id));
    assert_eq!(incident.impact, Impact::Critical);
    assert_eq!(incident.monitor_ids.len(), 1);

    let updated = admin
        .post_form(
            &format!("{url}/updates"),
            &[("status", "resolved"), ("message", "All good now.")],
        )
        .await;
    assert_eq!(updated.location(), Some(url.as_str()));
    admin
        .get(&url)
        .await
        .assert_contains("All good now.")
        .assert_contains("Requests are failing.");
    assert_eq!(
        app.db
            .store()
            .incident(incident.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        IncidentStatus::Resolved
    );
    let other = app
        .db
        .store()
        .declare_incident(
            &NewIncident {
                title: "Database outage".into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                message: "Investigating the database.".into(),
                monitors: vec!["db".parse().unwrap()],
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    admin
        .get("/admin/incidents")
        .await
        .assert_contains("API outage")
        .assert_contains("Delete API outage?")
        .assert_contains("Delete Database outage?")
        .assert_contains(&format!(r#"action="/admin/incidents/{}/delete""#, other.id))
        .assert_contains(&format!(r#"action="{url}/delete""#));

    let history_url = "/s/platform/incidents";
    app.client()
        .get(history_url)
        .await
        .assert_contains("API outage");
    let denied = app.client().post_form(&format!("{url}/delete"), &[]).await;
    assert_eq!(denied.location(), Some("/login"));
    assert!(
        app.db
            .store()
            .incident(incident.id)
            .await
            .unwrap()
            .is_some()
    );

    let deleted = admin.post_form(&format!("{url}/delete"), &[]).await;
    assert_eq!(deleted.location(), Some("/admin/incidents"));
    assert!(app.db.store().incident(other.id).await.unwrap().is_some());
    assert!(
        app.db
            .store()
            .incident(incident.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !app.client()
            .get(history_url)
            .await
            .body
            .contains("API outage")
    );
    assert_eq!(admin.get(&url).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        admin.post_form(&format!("{url}/delete"), &[]).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn incident_forms_explain_mistakes() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let reply = admin
        .post_form(
            "/admin/incidents",
            &[
                ("title", ""),
                ("impact", "major"),
                ("status", "investigating"),
                ("message", ""),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply
        .assert_contains("Give the incident a title.")
        .assert_contains("Write a first update")
        .assert_contains("Choose at least one affected monitor.");
}

// ── Admin: maintenance ───────────────────────────────────────────────────

#[tokio::test]
async fn admins_schedule_edit_and_cancel_maintenance() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let created = admin
        .post_form(
            "/admin/maintenance",
            &[
                ("title", "Kernel upgrade"),
                ("description", "Rolling reboots."),
                ("starts_at", "2031-03-01T22:00"),
                ("ends_at", "2031-03-01T23:30"),
                ("monitor_db", "on"),
            ],
        )
        .await;
    assert_eq!(
        created.location(),
        Some("/admin/maintenance"),
        "{}",
        created.body
    );
    let window = app.db.store().list_maintenances().await.unwrap().remove(0);
    assert_eq!(window.spec.starts_at.to_string(), "2031-03-01T22:00:00Z");
    assert_eq!(window.spec.monitors[0].as_str(), "db");

    let edit_url = format!("/admin/maintenance/{}", window.id);
    admin
        .get(&edit_url)
        .await
        .assert_contains("2031-03-01T22:00")
        .assert_contains("Rolling reboots.");
    admin
        .post_form(
            &edit_url,
            &[
                ("title", "Kernel upgrade"),
                ("description", ""),
                ("starts_at", "2031-03-01T22:00"),
                ("ends_at", "2031-03-02T01:00"),
                ("monitor_api", "on"),
                ("monitor_db", "on"),
            ],
        )
        .await;
    let window = app
        .db
        .store()
        .maintenance(window.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(window.spec.monitors.len(), 2);
    assert_eq!(window.spec.description, None);
    admin
        .get("/admin/maintenance")
        .await
        .assert_contains("Kernel upgrade");

    let deleted = admin.post_form(&format!("{edit_url}/delete"), &[]).await;
    assert_eq!(deleted.location(), Some("/admin/maintenance"));
    assert!(app.db.store().list_maintenances().await.unwrap().is_empty());
}

#[tokio::test]
async fn maintenance_forms_explain_mistakes() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let reply = admin
        .post_form(
            "/admin/maintenance",
            &[
                ("title", "Backwards"),
                ("starts_at", "2031-03-01T22:00"),
                ("ends_at", "2031-03-01T21:00"),
                ("monitor_api", "on"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply.assert_contains("The end must be after the start.");
    let bad_date = admin
        .post_form(
            "/admin/maintenance",
            &[("title", "x"), ("starts_at", "tomorrow"), ("ends_at", "")],
        )
        .await;
    bad_date.assert_contains("Use a date and time");
}

#[tokio::test]
async fn admins_schedule_recurring_maintenance() {
    let app = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let created = admin
        .post_form(
            "/admin/maintenance",
            &[
                ("title", "Weekly patching"),
                ("starts_at", "2031-03-01T22:00"),
                ("ends_at", "2031-03-01T23:00"),
                ("repeat", "weekly"),
                ("repeat_until", "2031-06-01T00:00"),
                ("monitor_db", "on"),
            ],
        )
        .await;
    assert_eq!(
        created.location(),
        Some("/admin/maintenance"),
        "{}",
        created.body
    );
    let window = app.db.store().list_maintenances().await.unwrap().remove(0);
    assert_eq!(window.spec.repeat, Some(uptime_domain::Repeat::Weekly));
    assert_eq!(
        window.spec.repeat_until.map(|t| t.to_string()).as_deref(),
        Some("2031-06-01T00:00:00Z")
    );
    admin
        .get("/admin/maintenance")
        .await
        .assert_contains("repeats weekly");
    admin
        .get(&format!("/admin/maintenance/{}", window.id))
        .await
        .assert_contains(r#"value="weekly" selected"#);

    let too_long = admin
        .post_form(
            "/admin/maintenance",
            &[
                ("title", "Too long"),
                ("starts_at", "2031-03-01T22:00"),
                ("ends_at", "2031-03-03T22:00"),
                ("repeat", "daily"),
                ("monitor_db", "on"),
            ],
        )
        .await;
    too_long.assert_contains("shorter than its repeat interval");
}

#[tokio::test]
async fn automatic_incidents_are_public_while_check_diagnostics_stay_in_admin() {
    let app = setup().await;
    let store = app.db.store();
    let monitor = store
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "api")
        .unwrap();
    let start = Timestamp::now();
    for (offset, state, failures) in [
        (0, uptime_domain::MonitorState::Pending, 1),
        (60, uptime_domain::MonitorState::Down, 2),
    ] {
        let at = start + SignedDuration::from_secs(offset);
        store
            .record_check(&uptime_store::CheckRecord {
                monitor_id: monitor.id,
                scheduled_for: at,
                checked_at: at,
                verdict: uptime_domain::Verdict {
                    health: uptime_domain::Health::Down,
                    latency: None,
                    status_code: Some(503),
                    reason: Some(uptime_domain::DownReason::UnexpectedStatus(503)),
                    response_body: Some("<script>private-upstream-response</script>".into()),
                },
                runtime: uptime_domain::Runtime {
                    state,
                    consecutive_failures: failures,
                },
                transition: (failures == 2).then_some(uptime_domain::Transition::WentDown),
                cert: None,
                next_run_at: at + SignedDuration::from_secs(60),
                region: "private-region".into(),
            })
            .await
            .unwrap();
    }
    let incident = store.list_incidents(10).await.unwrap().remove(0);
    let mut client = app.client();
    for url in [
        "/s/platform".to_owned(),
        "/s/platform/incidents".to_owned(),
        format!("/s/platform/incidents/{}", incident.id),
        "/s/platform/feed.atom".to_owned(),
    ] {
        let reply = client.get(&url).await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.assert_contains("api service is down");
        assert!(!reply.body.contains("private-upstream-response"));
        assert!(!reply.body.contains("private-region"));
        assert!(!reply.body.contains("unexpected status 503"));
    }
    let mut admin = app.signed_in("alice", 1001).await;
    let detail = admin
        .get(&format!("/admin/incidents/{}", incident.id))
        .await;
    detail.assert_contains("HTTP 503");
    detail.assert_contains("private-upstream-response");
    assert!(
        !detail
            .body
            .contains("<script>private-upstream-response</script>")
    );
    detail.assert_contains("private-region");

    let deleted = admin
        .post_form(&format!("/admin/incidents/{}/delete", incident.id), &[])
        .await;
    assert_eq!(deleted.location(), Some("/admin/incidents"));
    assert!(store.list_incidents(10).await.unwrap().is_empty());
    for url in [
        "/s/platform",
        "/s/platform/incidents",
        "/s/platform/feed.atom",
    ] {
        assert!(!client.get(url).await.body.contains("api service is down"));
    }
    assert_eq!(
        client
            .get(&format!("/s/platform/incidents/{}", incident.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
