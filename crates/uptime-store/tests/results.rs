#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{http_spec, t};
use jiff::civil::date;
use pretty_assertions::assert_eq;
use uptime_domain::{DownReason, FailureKind, Health, MonitorState, Runtime, Tally, Verdict};
use uptime_store::{CheckRecord, Monitor, Store};
use uptime_testkit::TestDb;

fn up_verdict(latency_ms: u64) -> Verdict {
    Verdict {
        health: Health::Up,
        latency: Some(Duration::from_millis(latency_ms)),
        status_code: Some(200),
        reason: None,
        response_body: None,
    }
}

fn timeout_verdict() -> Verdict {
    Verdict {
        health: Health::Down,
        latency: None,
        status_code: None,
        reason: Some(DownReason::Probe {
            kind: FailureKind::Timeout,
            message: "timed out after 10s".into(),
        }),
        response_body: None,
    }
}

async fn claimed_monitor(store: &Store) -> Monitor {
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    store.claim_due(t(1), 1, "w").await.unwrap();
    monitor
}

fn record(monitor: &Monitor, checked_at: i64, verdict: Verdict, runtime: Runtime) -> CheckRecord {
    CheckRecord {
        monitor_id: monitor.id,
        scheduled_for: t(checked_at - 1),
        checked_at: t(checked_at),
        verdict,
        runtime,
        transition: None,
        cert: None,
        next_run_at: t(checked_at + 59),
        region: "arn".into(),
    }
}

#[tokio::test]
async fn recording_updates_the_runtime_and_releases_the_claim() {
    let db = TestDb::new().await;
    let monitor = claimed_monitor(db.store()).await;
    let up = Runtime {
        state: MonitorState::Up,
        consecutive_failures: 0,
    };

    db.store()
        .record_check(&record(&monitor, 1, up_verdict(120), up))
        .await
        .unwrap();

    let runtime = db.store().runtime(monitor.id).await.unwrap().unwrap();
    assert_eq!(runtime.runtime, up);
    assert_eq!(runtime.next_run_at, t(60));
    assert_eq!(runtime.claimed_by, None);
    assert_eq!(runtime.last_checked_at, Some(t(1)));
    assert_eq!(runtime.last_latency_ms, Some(120));
    assert_eq!(runtime.last_status_code, Some(200));
    assert_eq!(runtime.last_error, None);
    assert_eq!(
        runtime.state_changed_at,
        Some(t(1)),
        "Unknown -> Up is a state change"
    );
}

#[tokio::test]
async fn state_changed_at_only_moves_when_the_state_changes() {
    let db = TestDb::new().await;
    let monitor = claimed_monitor(db.store()).await;
    let up = Runtime {
        state: MonitorState::Up,
        consecutive_failures: 0,
    };
    let pending = Runtime {
        state: MonitorState::Pending,
        consecutive_failures: 1,
    };

    db.store()
        .record_check(&record(&monitor, 1, up_verdict(10), up))
        .await
        .unwrap();
    db.store()
        .record_check(&record(&monitor, 61, up_verdict(10), up))
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .state_changed_at,
        Some(t(1))
    );

    db.store()
        .record_check(&record(&monitor, 121, timeout_verdict(), pending))
        .await
        .unwrap();
    let runtime = db.store().runtime(monitor.id).await.unwrap().unwrap();
    assert_eq!(runtime.state_changed_at, Some(t(121)));
    assert_eq!(
        runtime.last_error.as_deref(),
        Some("timeout: timed out after 10s")
    );
    assert_eq!(runtime.last_latency_ms, None);
}

#[tokio::test]
async fn recording_appends_raw_history_newest_first() {
    let db = TestDb::new().await;
    let monitor = claimed_monitor(db.store()).await;
    let up = Runtime {
        state: MonitorState::Up,
        consecutive_failures: 0,
    };
    let pending = Runtime {
        state: MonitorState::Pending,
        consecutive_failures: 1,
    };

    db.store()
        .record_check(&record(&monitor, 1, up_verdict(50), up))
        .await
        .unwrap();
    db.store()
        .record_check(&record(&monitor, 61, timeout_verdict(), pending))
        .await
        .unwrap();

    let history = db.store().recent_checks(monitor.id, 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].checked_at, t(61));
    assert_eq!(history[0].health, Health::Down);
    assert_eq!(history[0].state_after, MonitorState::Pending);
    assert_eq!(history[0].error_kind, Some(FailureKind::Timeout));
    assert_eq!(history[0].region, "arn");
    assert_eq!(history[1].checked_at, t(1));
    assert_eq!(history[1].latency_ms, Some(50));
    assert_eq!(history[1].status_code, Some(200));
}

#[tokio::test]
async fn daily_tally_accumulates_per_utc_day() {
    let db = TestDb::new().await;
    let monitor = claimed_monitor(db.store()).await;
    let up = Runtime {
        state: MonitorState::Up,
        consecutive_failures: 0,
    };
    let degraded = Runtime {
        state: MonitorState::Degraded,
        consecutive_failures: 0,
    };
    let down = Runtime {
        state: MonitorState::Down,
        consecutive_failures: 3,
    };

    // t(0) is 2027-01-15T08:00:00Z; t(86_400) is the next UTC day.
    db.store()
        .record_check(&record(&monitor, 1, up_verdict(100), up))
        .await
        .unwrap();
    db.store()
        .record_check(&record(&monitor, 61, up_verdict(300), degraded))
        .await
        .unwrap();
    db.store()
        .record_check(&record(&monitor, 121, timeout_verdict(), down))
        .await
        .unwrap();
    db.store()
        .record_check(&record(&monitor, 86_400, up_verdict(10), up))
        .await
        .unwrap();

    let first_day = db
        .store()
        .daily_tally(monitor.id, date(2027, 1, 15))
        .await
        .unwrap();
    let next_day = db
        .store()
        .daily_tally(monitor.id, date(2027, 1, 16))
        .await
        .unwrap();
    let empty_day = db
        .store()
        .daily_tally(monitor.id, date(2027, 1, 17))
        .await
        .unwrap();

    assert_eq!(
        first_day,
        Some(Tally {
            total: 3,
            up: 1,
            degraded: 1,
            down: 1,
            ..Default::default()
        })
    );
    assert_eq!(
        next_day,
        Some(Tally {
            total: 1,
            up: 1,
            ..Default::default()
        })
    );
    assert_eq!(empty_day, None);
}

#[tokio::test]
async fn checks_since_returns_the_window_oldest_first() {
    let db = TestDb::new().await;
    let monitor = claimed_monitor(db.store()).await;
    let up = Runtime {
        state: MonitorState::Up,
        consecutive_failures: 0,
    };
    for (at, latency) in [(10, 30), (70, 50), (130, 40)] {
        db.store()
            .record_check(&record(&monitor, at, up_verdict(latency), up))
            .await
            .unwrap();
    }

    let window = db.store().checks_since(monitor.id, t(60)).await.unwrap();

    assert_eq!(
        window
            .iter()
            .map(|c| (c.checked_at, c.latency_ms))
            .collect::<Vec<_>>(),
        vec![(t(70), Some(50)), (t(130), Some(40))]
    );
    assert_eq!(window[0].state_after, MonitorState::Up);
}
