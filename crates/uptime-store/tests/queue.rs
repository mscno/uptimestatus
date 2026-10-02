#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::collections::HashMap;

use common::{http_spec, t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{MonitorSpec, MonitorState, Runtime};
use uptime_testkit::TestDb;

#[tokio::test]
async fn claims_only_due_active_monitors() {
    let db = TestDb::new().await;
    let store = db.store();
    let due = store.create_monitor(&http_spec("due"), t(0)).await.unwrap();
    store
        .create_monitor(&tcp_spec("later"), t(120))
        .await
        .unwrap();
    store
        .create_monitor(
            &MonitorSpec {
                active: false,
                tags: Vec::new(),
                group: None,
                ..tcp_spec("paused")
            },
            t(0),
        )
        .await
        .unwrap();

    let claims = store.claim_due(t(60), 10, "worker-a").await.unwrap();

    assert_eq!(claims.len(), 1);
    let claim = &claims[0];
    assert_eq!(claim.monitor, due);
    assert_eq!(claim.scheduled_for, t(0));
    assert_eq!(
        claim.runtime,
        Runtime {
            state: MonitorState::Unknown,
            consecutive_failures: 0
        }
    );
}

#[tokio::test]
async fn claiming_pushes_next_run_out_by_timeout_plus_grace() {
    let db = TestDb::new().await;
    let store = db.store();
    // http_spec has a 10s timeout.
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();

    store.claim_due(t(30), 10, "worker-a").await.unwrap();
    let runtime = store.runtime(monitor.id).await.unwrap().unwrap();

    assert_eq!(runtime.scheduled_for, Some(t(0)));
    assert_eq!(runtime.next_run_at, t(30 + 10 + 60));
    assert_eq!(runtime.claimed_by.as_deref(), Some("worker-a"));
}

#[tokio::test]
async fn a_claimed_slot_is_not_claimed_again_until_the_watchdog_fires() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&http_spec("api"), t(0)).await.unwrap();

    assert_eq!(store.claim_due(t(1), 10, "a").await.unwrap().len(), 1);
    assert_eq!(store.claim_due(t(2), 10, "b").await.unwrap().len(), 0);
    // Watchdog: 1 + 10s timeout + 60s grace = 71.
    assert_eq!(store.claim_due(t(70), 10, "b").await.unwrap().len(), 0);
    let reclaimed = store.claim_due(t(71), 10, "b").await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].scheduled_for, t(71));
}

#[tokio::test]
async fn claims_oldest_first_and_respects_the_limit() {
    let db = TestDb::new().await;
    let store = db.store();
    for (key, due_at) in [("c", 30), ("a", 10), ("b", 20)] {
        store
            .create_monitor(&tcp_spec(key), t(due_at))
            .await
            .unwrap();
    }

    let first: Vec<_> = store
        .claim_due(t(100), 2, "w")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.monitor.spec.key.to_string())
        .collect();
    let rest: Vec<_> = store
        .claim_due(t(100), 2, "w")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.monitor.spec.key.to_string())
        .collect();

    assert_eq!(first, ["a", "b"]);
    assert_eq!(rest, ["c"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claimers_never_claim_the_same_slot_twice() {
    const MONITORS: usize = 60;
    const CLAIMERS: usize = 8;
    let db = TestDb::new().await;
    for i in 0..MONITORS {
        db.store()
            .create_monitor(&tcp_spec(&format!("m{i}")), t(0))
            .await
            .unwrap();
    }

    let tasks: Vec<_> = (0..CLAIMERS)
        .map(|n| {
            let store = db.store().clone();
            tokio::spawn(async move {
                let mut claimed = Vec::new();
                loop {
                    let batch = store
                        .claim_due(t(10), 3, &format!("claimer-{n}"))
                        .await
                        .unwrap();
                    if batch.is_empty() {
                        break claimed;
                    }
                    claimed.extend(batch.into_iter().map(|c| c.monitor.id));
                }
            })
        })
        .collect();

    let mut claims_per_monitor = HashMap::new();
    for task in tasks {
        for id in task.await.unwrap() {
            *claims_per_monitor.entry(id).or_insert(0) += 1;
        }
    }

    assert_eq!(claims_per_monitor.len(), MONITORS, "every monitor claimed");
    assert!(
        claims_per_monitor.values().all(|&n| n == 1),
        "no monitor claimed twice: {claims_per_monitor:?}"
    );
}
