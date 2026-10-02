#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

use std::{sync::Arc, time::Duration};

use jiff::{SignedDuration, Timestamp, Unit};
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;
use uptime_domain::{
    CheckSpec, Health, MonitorId, MonitorSpec, MonitorState, Runtime, TcpCheck, Verdict,
};
use uptime_runtime::Janitor;
use uptime_store::{CheckRecord, Store};
use uptime_testkit::TestDb;

const DAY: i64 = 24 * 60 * 60;

async fn monitor(store: &Store) -> MonitorId {
    let spec = MonitorSpec {
        key: "db".parse().unwrap(),
        name: "DB".into(),
        check: CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432)),
        policy: Default::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    };
    store
        .create_monitor(&spec, Timestamp::now())
        .await
        .unwrap()
        .id
}

async fn record(store: &Store, id: MonitorId, at: Timestamp) {
    let record = CheckRecord {
        monitor_id: id,
        scheduled_for: at,
        checked_at: at,
        verdict: Verdict {
            health: Health::Up,
            latency: None,
            status_code: None,
            reason: None,
            response_body: None,
        },
        runtime: Runtime {
            state: MonitorState::Up,
            consecutive_failures: 0,
        },
        transition: None,
        cert: None,
        next_run_at: at,
        region: "test".into(),
    };
    store.record_check(&record).await.unwrap();
}

fn days_ago(now: Timestamp, days: i64) -> Timestamp {
    now - SignedDuration::from_secs(days * DAY)
}

#[tokio::test]
async fn prunes_results_older_than_the_retention() {
    let db = TestDb::new().await;
    let now = Timestamp::now().round(Unit::Second).unwrap();
    let id = monitor(db.store()).await;
    for days in [30, 15, 13, 1] {
        record(db.store(), id, days_ago(now, days)).await;
    }
    let janitor = Janitor::new(
        db.store().clone(),
        Duration::from_secs(14 * DAY as u64),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now));

    assert_eq!(janitor.run_once().await.unwrap(), 2);

    let left: Vec<_> = db
        .store()
        .recent_checks(id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.checked_at)
        .collect();
    assert_eq!(left, [days_ago(now, 1), days_ago(now, 13)]);
}

#[tokio::test]
async fn the_loop_prunes_on_start_and_stops_on_shutdown() {
    let db = TestDb::new().await;
    let id = monitor(db.store()).await;
    record(db.store(), id, days_ago(Timestamp::now(), 30)).await;
    let janitor = Janitor::new(
        db.store().clone(),
        Duration::from_secs(14 * DAY as u64),
        Duration::from_secs(3600),
    );
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(janitor.run(shutdown.clone()));

    tokio::time::timeout(Duration::from_secs(5), async {
        while !db.store().recent_checks(id, 1).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("old results pruned");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn expired_sessions_are_pruned_too() {
    use uptime_store::{GithubIdentity, NewSession};
    let db = TestDb::new().await;
    let now = Timestamp::now().round(Unit::Second).unwrap();
    let identity = GithubIdentity {
        github_id: 1,
        login: "alice".into(),
        name: None,
        avatar_url: None,
    };
    let admin = db.store().record_admin_login(&identity, now).await.unwrap();
    for (hash, expires) in [
        (1u8, days_ago(now, 1)),
        (2, now + SignedDuration::from_hours(1)),
    ] {
        let session = NewSession {
            token_hash: [hash; 32],
            user_id: admin.id,
            created_at: days_ago(now, 2),
            expires_at: expires,
            user_agent: None,
        };
        db.store().create_session(&session).await.unwrap();
    }
    let janitor = Janitor::new(
        db.store().clone(),
        Duration::from_secs(14 * DAY as u64),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now));

    janitor.run_once().await.unwrap();

    let max_age = SignedDuration::from_hours(24 * 30);
    assert!(
        db.store()
            .session_admin(&[1; 32], days_ago(now, 2), max_age)
            .await
            .unwrap()
            .is_none(),
        "expired session pruned"
    );
    assert!(
        db.store()
            .session_admin(&[2; 32], now, max_age)
            .await
            .unwrap()
            .is_some()
    );
}
