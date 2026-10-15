#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The audit trail: console and API changes are recorded without secrets,
//! and the console lists and filters them.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use serde_json::json;
use uptime_domain::{
    CheckPolicy, CheckSpec, DownReason, Health, MonitorId, MonitorSpec, MonitorState, Runtime,
    TcpCheck, TokenScope, Verdict,
};
use uptime_store::{AuditEvent, AuditQuery, NewApiToken};

async fn events(app: &TestApp, action: &str) -> Vec<AuditEvent> {
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

fn form(name: &str, password: &str) -> Vec<(&'static str, String)> {
    vec![
        ("key", "orders".to_owned()),
        ("name", name.to_owned()),
        ("check_type", "http".to_owned()),
        ("url", "https://orders.example.com/health".to_owned()),
        ("method", "GET".to_owned()),
        ("accepted_status", "200-299".to_owned()),
        ("interval", "1m".to_owned()),
        ("retry_interval", "20s".to_owned()),
        ("timeout", "5s".to_owned()),
        ("retries", "1".to_owned()),
        ("active", "on".to_owned()),
        ("auth_type", "basic".to_owned()),
        ("auth_username", "ops".to_owned()),
        ("auth_password", password.to_owned()),
    ]
}

fn fields<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[tokio::test]
async fn console_monitor_changes_are_recorded_without_secrets() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let created = admin
        .post_form("/admin/monitors", &fields(&form("Orders", "first-secret")))
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let id = created
        .location()
        .unwrap()
        .trim_start_matches("/admin/monitors/")
        .to_owned();
    let updated = admin
        .post_form(
            &format!("/admin/monitors/{id}"),
            &fields(&form("Orders API", "second-secret")),
        )
        .await;
    assert_eq!(updated.status, StatusCode::SEE_OTHER, "{}", updated.body);
    admin
        .post_form(&format!("/admin/monitors/{id}/pause"), &[])
        .await;

    let update = events(&app, "monitor.update").await.remove(0);
    assert_eq!(update.actor, "alice");
    assert_eq!(update.target.as_ref().unwrap().id, id);
    assert_eq!(
        update.summary,
        "Updated monitor Orders API (check.auth.password, name)"
    );
    assert_eq!(
        update.details,
        json!({ "changed": {
            "check.auth.password": { "from": "***", "to": "***" },
            "name": { "from": "Orders", "to": "Orders API" },
        }})
    );
    let create = events(&app, "monitor.create").await.remove(0);
    assert_eq!(create.details["monitor"]["name"], "Orders");
    assert_eq!(events(&app, "monitor.pause").await.len(), 1);
    for event in events(&app, "monitor").await {
        let text = event.details.to_string();
        assert!(
            !text.contains("first-secret") && !text.contains("second-secret"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn deleting_and_excusing_incidents_is_recorded_with_the_reason() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let monitor = store
        .create_monitor(
            &MonitorSpec {
                key: "api".parse().unwrap(),
                name: "API".into(),
                check: CheckSpec::Tcp(TcpCheck::connect("api.example.com", 443)),
                policy: CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let start = Timestamp::now() - SignedDuration::from_mins(5);
    for (offset, state, failures) in [
        (0, MonitorState::Pending, 1),
        (20, MonitorState::Down, 2),
        (40, MonitorState::Up, 0),
    ] {
        let at = start + SignedDuration::from_secs(offset);
        let failed = failures > 0;
        store
            .record_check(&uptime_store::CheckRecord {
                monitor_id: monitor.id,
                scheduled_for: at,
                checked_at: at,
                verdict: Verdict {
                    health: if failed { Health::Down } else { Health::Up },
                    latency: None,
                    status_code: Some(if failed { 502 } else { 200 }),
                    reason: failed.then_some(DownReason::UnexpectedStatus(502)),
                    response_body: failed.then(|| "private upstream body".into()),
                },
                runtime: Runtime {
                    state,
                    consecutive_failures: failures,
                },
                transition: None,
                cert: None,
                next_run_at: at + SignedDuration::from_secs(20),
                region: "fra".into(),
            })
            .await
            .unwrap();
    }
    let incident = store.list_incidents(1).await.unwrap().remove(0);
    let mut admin = app.signed_in("alice", 1001).await;
    let base = format!("/admin/incidents/{}", incident.id);
    admin
        .get(&base)
        .await
        .assert_contains("Excuse from uptime")
        .assert_contains(r#"name="reason""#);
    admin
        .post_form(
            &format!("{base}/excuse"),
            &[("reason", "the checker's network was down")],
        )
        .await;
    admin.post_form(&format!("{base}/delete"), &[]).await;

    let excuse = events(&app, "uptime.excuse").await.remove(0);
    assert_eq!(excuse.actor, "alice");
    assert_eq!(excuse.details["reason"], "the checker's network was down");
    assert_eq!(excuse.details["checks"], 2);
    assert_eq!(excuse.details["monitors"], json!([monitor.id.0]));
    let deleted = events(&app, "incident.delete").await.remove(0);
    assert_eq!(deleted.summary, "Deleted incident API is down");
    assert_eq!(deleted.details["kind"], "auto");

    // The monitor form takes a reason too.
    admin
        .post_form(
            &format!("/admin/monitors/{}/excuse", monitor.id),
            &[
                ("from", &start.strftime("%Y-%m-%dT%H:%M").to_string()),
                (
                    "to",
                    &(start + SignedDuration::from_mins(2))
                        .strftime("%Y-%m-%dT%H:%M")
                        .to_string(),
                ),
                ("action", "restore"),
                ("reason", "it was real after all"),
            ],
        )
        .await;
    let restore = events(&app, "uptime.restore").await.remove(0);
    assert_eq!(restore.details["reason"], "it was real after all");
    assert_eq!(restore.details["checks"], 2);
    for event in events(&app, "uptime").await {
        assert!(!event.details.to_string().contains("private upstream body"));
    }
}

#[tokio::test]
async fn api_writes_are_recorded_as_their_token() {
    let app = TestApp::new().await;
    let secret = uptime_web::new_api_token();
    app.db
        .store()
        .create_api_token(
            &NewApiToken {
                name: "deploy".into(),
                token_hash: uptime_web::hash_api_token(&secret),
                scope: TokenScope::Write,
                created_by: "alice".into(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut client = app.client();
    let body = json!({
        "name": "Cron",
        "check": {"type": "push", "token": "push-secret-token"},
    });
    let put = client
        .api("PUT", "/api/v1/monitors/cron", Some(&secret), Some(body))
        .await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.body);
    let delete = client
        .api("DELETE", "/api/v1/monitors/cron", Some(&secret), None)
        .await;
    assert_eq!(delete.status, StatusCode::NO_CONTENT);

    let recorded = events(&app, "monitor").await;
    assert_eq!(
        recorded
            .iter()
            .map(|e| (e.actor.as_str(), e.action.as_str()))
            .collect::<Vec<_>>(),
        [
            ("api:deploy", "monitor.delete"),
            ("api:deploy", "monitor.create")
        ]
    );
    for event in &recorded {
        assert!(!event.details.to_string().contains("push-secret-token"));
        assert_eq!(event.details["monitor"]["check"]["token"], "***");
    }
}

#[tokio::test]
async fn the_audit_page_lists_and_filters_events() {
    let app = TestApp::new().await;
    let store = app.db.store();
    let monitor = store
        .create_monitor(
            &MonitorSpec {
                key: "db".parse().unwrap(),
                name: "Database".into(),
                check: CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432)),
                policy: CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_form(&format!("/admin/monitors/{}/pause", monitor.id), &[])
        .await;
    let gone = store
        .create_monitor(
            &MonitorSpec {
                key: "old".parse().unwrap(),
                name: "Old".into(),
                ..store.monitor(monitor.id).await.unwrap().unwrap().spec
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    admin
        .post_form(&format!("/admin/monitors/{}/delete", gone.id), &[])
        .await;

    let page = admin.get("/admin/audit").await;
    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains("Audit trail")
        .assert_contains(r#"href="/admin/audit""#)
        .assert_contains("Paused monitor Database")
        .assert_contains(&format!(r#"href="/admin/monitors/{}""#, monitor.id))
        .assert_contains("Deleted monitor Old (old)")
        .assert_contains(&format!("monitor {} (deleted)", gone.id))
        .assert_contains("auth.login");

    let filtered = admin
        .get(&format!(
            "/admin/audit?action=monitor&target=monitor:{}",
            MonitorId(monitor.id.0)
        ))
        .await;
    filtered.assert_contains("Paused monitor Database");
    assert!(!filtered.body.contains("Deleted monitor Old"));
    assert!(!filtered.body.contains("signed in"));
    let by_actor = admin.get("/admin/audit?actor=nobody").await;
    by_actor.assert_contains("No events.");

    // Signed-out visitors are sent to sign in.
    let anonymous = app.client().get("/admin/audit").await;
    assert!(anonymous.status.is_redirection());
    assert_eq!(anonymous.location(), Some("/login"));
}
