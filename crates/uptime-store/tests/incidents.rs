#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{http_spec, t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{
    DownReason, FailureKind, Health, Impact, IncidentKind, IncidentStatus, MonitorId, MonitorState,
    Runtime, Transition, Verdict,
};
use uptime_store::{CheckRecord, NewIncident, Store, StoreError};
use uptime_testkit::TestDb;

fn declared(title: &str, monitors: &[&str]) -> NewIncident {
    NewIncident {
        title: title.into(),
        impact: Impact::Major,
        status: IncidentStatus::Investigating,
        message: "We are looking into elevated error rates.".into(),
        monitors: monitors.iter().map(|k| k.parse().unwrap()).collect(),
    }
}

async fn record(
    store: &Store,
    id: MonitorId,
    at: i64,
    state: MonitorState,
    transition: Option<Transition>,
) {
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if state == MonitorState::Down {
                    Health::Down
                } else {
                    Health::Up
                },
                latency: Some(Duration::from_millis(5)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state,
                consecutive_failures: if state == MonitorState::Down { 2 } else { 0 },
            },
            transition,
            cert: None,
            next_run_at: t(at + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn declared_incidents_carry_a_timeline() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();

    let incident = store
        .declare_incident(&declared("Elevated errors", &["api"]), t(10))
        .await
        .unwrap();
    assert_eq!(incident.kind, IncidentKind::Manual);
    assert_eq!(incident.monitor_ids, [api.id]);
    assert_eq!(incident.updates.len(), 1);

    store
        .post_incident_update(
            incident.id,
            IncidentStatus::Identified,
            "A bad deploy.",
            t(20),
        )
        .await
        .unwrap();
    let resolved = store
        .post_incident_update(incident.id, IncidentStatus::Resolved, "Rolled back.", t(30))
        .await
        .unwrap();

    assert_eq!(resolved.status, IncidentStatus::Resolved);
    assert_eq!(resolved.resolved_at, Some(t(30)));
    let bodies: Vec<&str> = resolved.updates.iter().map(|u| u.body.as_str()).collect();
    assert_eq!(
        bodies,
        [
            "Rolled back.",
            "A bad deploy.",
            "We are looking into elevated error rates."
        ]
    );

    let reopened = store
        .post_incident_update(
            incident.id,
            IncidentStatus::Monitoring,
            "Seeing errors again.",
            t(40),
        )
        .await
        .unwrap();
    assert_eq!(reopened.resolved_at, None);
}

#[tokio::test]
async fn unknown_incidents_and_monitors_are_errors() {
    let db = TestDb::new().await;
    let store = db.store();
    assert!(matches!(
        store
            .declare_incident(&declared("x", &["ghost"]), t(0))
            .await,
        Err(StoreError::UnknownMonitors(_))
    ));
    assert!(matches!(
        store
            .post_incident_update(4242, IncidentStatus::Resolved, "done", t(0))
            .await,
        Err(StoreError::IncidentNotFound(4242))
    ));
}

#[tokio::test]
async fn going_down_opens_and_recovering_resolves_an_automatic_incident() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();

    record(
        store,
        api.id,
        100,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    record(
        store,
        api.id,
        160,
        MonitorState::Down,
        Some(Transition::Resend),
    )
    .await;
    let open = store.list_incidents(10).await.unwrap();
    assert_eq!(open.len(), 1, "resends do not open another");
    assert_eq!(open[0].kind, IncidentKind::Auto);
    assert_eq!(open[0].title, "TCP api is down");
    assert_eq!(open[0].resolved_at, None);

    record(
        store,
        api.id,
        400,
        MonitorState::Up,
        Some(Transition::Recovered),
    )
    .await;

    let incident = store.incident(open[0].id).await.unwrap().unwrap();
    assert_eq!(incident.status, IncidentStatus::Resolved);
    assert_eq!(incident.resolved_at, Some(t(400)));
    assert_eq!(incident.updates[0].body, "TCP api recovered after 5m.");
}

#[tokio::test]
async fn status_pages_see_recent_incidents_for_their_monitors() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let web = store.create_monitor(&tcp_spec("web"), t(0)).await.unwrap();
    let open = store
        .declare_incident(&declared("Open", &["api"]), t(10))
        .await
        .unwrap();
    let old = store
        .declare_incident(&declared("Old", &["api"]), t(0))
        .await
        .unwrap();
    store
        .post_incident_update(old.id, IncidentStatus::Resolved, "Fixed", t(5))
        .await
        .unwrap();
    store
        .declare_incident(&declared("Elsewhere", &["web"]), t(10))
        .await
        .unwrap();
    record(
        store,
        api.id,
        20,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;

    let seen: Vec<String> = store
        .public_incidents(&[api.id], t(100))
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.title)
        .collect();
    assert_eq!(
        seen,
        ["TCP api is down", "Open"],
        "no old or unrelated incidents"
    );

    let with_history: Vec<String> = store
        .public_incidents(&[api.id, web.id], t(0))
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.title)
        .collect();
    assert_eq!(with_history.len(), 4);
    assert!(open.id > 0);
}

#[tokio::test]
async fn incidents_can_be_deleted() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let incident = store
        .declare_incident(&declared("Oops", &["api"]), t(0))
        .await
        .unwrap();
    assert!(store.delete_incident(incident.id).await.unwrap());
    assert_eq!(store.incident(incident.id).await.unwrap(), None);
}

#[tokio::test]
async fn incident_history_includes_automatic_incidents_started_in_a_range() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    store.create_monitor(&tcp_spec("web"), t(0)).await.unwrap();
    for (title, at, monitors) in [
        ("Before", 0, &["api"][..]),
        ("First", 100, &["api"]),
        ("Second", 150, &["api", "web"]),
        ("Elsewhere", 150, &["web"]),
        ("After", 200, &["api"]),
    ] {
        store
            .declare_incident(&declared(title, monitors), t(at))
            .await
            .unwrap();
    }
    // An automatic incident inside the range is public too.
    record(
        store,
        api.id,
        120,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;

    let titles: Vec<String> = store
        .incident_history(&[api.id], t(100), t(200))
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.title)
        .collect();

    assert_eq!(
        titles,
        ["Second", "TCP api is down", "First"],
        "newest first, [from, to)"
    );
    assert!(
        store
            .incident_history(&[], t(0), t(1000))
            .await
            .unwrap()
            .is_empty()
    );
}

/// One HTTP check: `Some(code)` answered with that status, `None` was refused.
async fn http_check(
    store: &Store,
    id: MonitorId,
    at: i64,
    (state, failures): (MonitorState, u32),
    code: Option<u16>,
) {
    let up = state == MonitorState::Up;
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if up { Health::Up } else { Health::Down },
                latency: code.map(|_| Duration::from_millis(40)),
                status_code: code,
                reason: match (up, code) {
                    (true, _) => None,
                    (false, Some(code)) => Some(DownReason::UnexpectedStatus(code)),
                    (false, None) => Some(DownReason::Probe {
                        kind: FailureKind::Refused,
                        message: "tcp connect error: Connection refused (os error 111)".into(),
                    }),
                },
                response_body: (!up && code.is_some()).then(|| format!("upstream said {at}")),
            },
            runtime: Runtime {
                state,
                consecutive_failures: failures,
            },
            transition: None,
            cert: None,
            next_run_at: t(at + 20),
            region: "fra".into(),
        })
        .await
        .unwrap();
}

/// Up at 0; fails at 60 (pending), 80 (confirmed) and 100; recovers at 160.
async fn outage(store: &Store, id: MonitorId) {
    http_check(store, id, 0, (MonitorState::Up, 0), Some(200)).await;
    http_check(store, id, 60, (MonitorState::Pending, 1), Some(503)).await;
    http_check(store, id, 80, (MonitorState::Down, 2), Some(502)).await;
    http_check(store, id, 100, (MonitorState::Down, 3), None).await;
    http_check(store, id, 160, (MonitorState::Up, 0), Some(200)).await;
}

fn summary(checks: &[uptime_store::StoredCheck]) -> Vec<(i64, Health, Option<u16>)> {
    checks
        .iter()
        .map(|c| {
            (
                c.checked_at.as_second() - t(0).as_second(),
                c.health,
                c.status_code,
            )
        })
        .collect()
}

#[tokio::test]
async fn incident_checks_cover_the_outage_through_recovery() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    outage(store, api.id).await;
    let incident = store.list_incidents(10).await.unwrap().remove(0);
    assert_eq!(incident.started_at, t(80));
    assert_eq!(incident.resolved_at, Some(t(160)));

    let checks = store.incident_checks(&incident).await.unwrap();
    let expected = [
        (60, Health::Down, Some(503)),
        (80, Health::Down, Some(502)),
        (100, Health::Down, None),
        (160, Health::Up, Some(200)),
    ];
    assert_eq!(summary(&checks), expected, "oldest first, recovery last");
    assert_eq!(checks[0].response_body.as_deref(), Some("upstream said 60"));
    assert_eq!(checks[2].error_kind, Some(FailureKind::Refused));
    assert_eq!(
        checks[2].error.as_deref(),
        Some("refused: tcp connect error: Connection refused (os error 111)")
    );

    // The public timeline stays narrative: opened and recovered.
    let public = store.public_incident(incident.id).await.unwrap().unwrap();
    let bodies: Vec<&str> = public.updates.iter().map(|u| u.body.as_str()).collect();
    assert_eq!(
        bodies,
        [
            "Monitor api recovered after 1m 20s.",
            "Monitor api failed its checks."
        ]
    );

    // Diagnostics outlive raw check retention.
    store.prune_checks_before(t(1000), 100).await.unwrap();
    let incident = store.incident(incident.id).await.unwrap().unwrap();
    assert_eq!(
        summary(&store.incident_checks(&incident).await.unwrap()),
        expected
    );
}

#[tokio::test]
async fn incidents_without_stored_diagnostics_use_raw_check_history() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let web = store.create_monitor(&http_spec("web"), t(0)).await.unwrap();
    outage(store, api.id).await;
    // Unrelated checks and later failures stay out.
    http_check(store, web.id, 90, (MonitorState::Pending, 1), Some(500)).await;
    http_check(store, api.id, 220, (MonitorState::Pending, 1), Some(500)).await;
    // Incidents opened before 0.1.2 kept no per-check diagnostics.
    let (client, connection) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    client
        .batch_execute("DELETE FROM incident_updates WHERE check_json IS NOT NULL")
        .await
        .unwrap();
    let incident = store.list_incidents(10).await.unwrap().remove(0);
    assert!(incident.updates.iter().all(|u| u.check.is_none()));

    let checks = store.incident_checks(&incident).await.unwrap();
    assert_eq!(
        summary(&checks),
        [
            (60, Health::Down, Some(503)),
            (80, Health::Down, Some(502)),
            (100, Health::Down, None),
            (160, Health::Up, Some(200)),
        ]
    );

    // Declared incidents carry no checks.
    let declared = store
        .declare_incident(&declared("Planned", &["api"]), t(300))
        .await
        .unwrap();
    assert!(store.incident_checks(&declared).await.unwrap().is_empty());
}
