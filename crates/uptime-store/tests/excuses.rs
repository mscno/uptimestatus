#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Admins excuse failed checks that should not count against uptime.

mod common;

use std::time::Duration;

use common::{http_spec, t};
use pretty_assertions::assert_eq;
use uptime_domain::{Health, MonitorId, MonitorState, Runtime, Tally, Verdict};
use uptime_store::{CheckRecord, Store};
use uptime_testkit::TestDb;

async fn check(store: &Store, id: MonitorId, at: i64, state: MonitorState) {
    let failed = matches!(state, MonitorState::Pending | MonitorState::Down);
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if failed { Health::Down } else { Health::Up },
                latency: Some(Duration::from_millis(30)),
                status_code: Some(if failed { 503 } else { 200 }),
                reason: failed.then_some(uptime_domain::DownReason::UnexpectedStatus(503)),
                response_body: None,
            },
            runtime: Runtime {
                state,
                consecutive_failures: u32::from(failed),
            },
            transition: None,
            cert: None,
            next_run_at: t(at + 60),
            region: "fra".into(),
        })
        .await
        .unwrap();
}

async fn today(store: &Store, id: MonitorId) -> Tally {
    store
        .daily_tally(id, t(0).to_zoned(jiff::tz::TimeZone::UTC).date())
        .await
        .unwrap()
        .unwrap()
}

fn states(checks: &[uptime_store::WindowCheck]) -> Vec<MonitorState> {
    checks.iter().map(|c| c.state_after).collect()
}

#[tokio::test]
async fn excused_failures_count_as_up_until_counted_again() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let web = store.create_monitor(&http_spec("web"), t(0)).await.unwrap();
    for (at, state) in [
        (0, MonitorState::Up),
        (60, MonitorState::Pending),
        (80, MonitorState::Down),
        (100, MonitorState::Down),
        (120, MonitorState::Up),
        (180, MonitorState::Down),
    ] {
        check(store, api.id, at, state).await;
    }
    check(store, web.id, 80, MonitorState::Down).await;
    let before = Tally {
        total: 6,
        up: 2,
        degraded: 0,
        pending: 1,
        down: 3,
        maintenance: 0,
    };
    assert_eq!(today(store, api.id).await, before);

    // Excuse the outage between 60 and 100, not the later failure.
    let excused = store
        .excuse_checks(api.id, t(60), t(100), t(500))
        .await
        .unwrap();
    assert_eq!(excused, 3);
    assert_eq!(
        today(store, api.id).await,
        Tally {
            up: 5,
            pending: 0,
            down: 1,
            ..before
        }
    );
    assert_eq!(
        states(&store.checks_since(api.id, t(0)).await.unwrap()),
        [
            MonitorState::Up,
            MonitorState::Up,
            MonitorState::Up,
            MonitorState::Up,
            MonitorState::Up,
            MonitorState::Down
        ]
    );
    // Diagnostics stay, marked as excused.
    let recent = store.recent_checks(api.id, 10).await.unwrap();
    let marks: Vec<(MonitorState, bool)> =
        recent.iter().map(|c| (c.state_after, c.excused)).collect();
    assert_eq!(
        marks,
        [
            (MonitorState::Down, false),
            (MonitorState::Up, false),
            (MonitorState::Down, true),
            (MonitorState::Down, true),
            (MonitorState::Pending, true),
            (MonitorState::Up, false),
        ]
    );
    // Other monitors are untouched; excusing again changes nothing.
    assert_eq!(today(store, web.id).await.down, 1);
    assert_eq!(
        store
            .excuse_checks(api.id, t(0), t(100), t(600))
            .await
            .unwrap(),
        0
    );

    // Counting them again restores the original tally.
    assert_eq!(
        store.restore_checks(api.id, t(0), t(1000)).await.unwrap(),
        3
    );
    assert_eq!(today(store, api.id).await, before);
    assert!(
        store
            .recent_checks(api.id, 10)
            .await
            .unwrap()
            .iter()
            .all(|c| !c.excused)
    );
    assert_eq!(
        store.restore_checks(api.id, t(0), t(1000)).await.unwrap(),
        0
    );
}
