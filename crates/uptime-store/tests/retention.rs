#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{http_spec, t};
use pretty_assertions::assert_eq;
use uptime_domain::{Health, MonitorState, Runtime, Verdict};
use uptime_store::CheckRecord;
use uptime_testkit::TestDb;

async fn record_at(db: &TestDb, monitor: uptime_domain::MonitorId, at: i64) {
    db.store()
        .record_check(&CheckRecord {
            monitor_id: monitor,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: Health::Up,
                latency: Some(Duration::from_millis(5)),
                status_code: Some(200),
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Up,
                consecutive_failures: 0,
            },
            transition: None,
            cert: None,
            next_run_at: t(at + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn prunes_only_results_older_than_the_cutoff_in_batches() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();
    for at in [0, 10, 20, 30, 40, 100, 200] {
        record_at(&db, monitor.id, at).await;
    }

    let deleted = db.store().prune_checks_before(t(50), 2).await.unwrap();

    assert_eq!(deleted, 5);
    let left: Vec<_> = db
        .store()
        .recent_checks(monitor.id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.checked_at)
        .collect();
    assert_eq!(left, [t(200), t(100)]);
}

#[tokio::test]
async fn pruning_keeps_daily_counters() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();
    record_at(&db, monitor.id, 0).await;

    db.store().prune_checks_before(t(1_000), 100).await.unwrap();

    let day = t(0).to_zoned(jiff::tz::TimeZone::UTC).date();
    assert_eq!(
        db.store()
            .daily_tally(monitor.id, day)
            .await
            .unwrap()
            .unwrap()
            .total,
        1
    );
}
