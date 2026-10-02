#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{
    Health, Impact, IncidentKind, IncidentStatus, MonitorId, MonitorState, Runtime, Transition,
    Verdict,
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
