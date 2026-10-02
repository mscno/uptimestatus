#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Two instances sharing events over the Postgres bus.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use jiff::Timestamp;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uptime_domain::{MonitorId, MonitorState, Transition};
use uptime_runtime::{CheckCompleted, Cluster, EventBus, Handlers};
use uptime_store::bus::BusMessage;
use uptime_testkit::TestDb;

fn event() -> CheckCompleted {
    CheckCompleted {
        monitor_id: MonitorId(3),
        key: "api".parse().unwrap(),
        previous: MonitorState::Up,
        state: MonitorState::Down,
        transition: Some(Transition::WentDown),
        latency_ms: None,
        checked_at: Timestamp::from_second(1_800_000_000).unwrap(),
    }
}

#[tokio::test]
async fn checks_on_one_instance_reach_dashboards_on_another() {
    let db = TestDb::new().await;
    let shutdown = CancellationToken::new();
    let worker = Cluster::new(db.store().clone(), "worker");
    let web = Cluster::new(db.store().clone(), "web");
    let web_events = EventBus::default();
    let mut dashboard = web_events.subscribe();
    tokio::spawn(web.listen(
        db.url().to_owned(),
        Handlers {
            events: Some(web_events.clone()),
            ..Handlers::default()
        },
        shutdown.clone(),
    ));
    let worker_events = EventBus::default().with_forwarder(worker.forwarder());
    let mut local = worker_events.subscribe();

    // The listener needs a moment to LISTEN; publish until it arrives.
    let received = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            worker_events.publish(event());
            if let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_millis(300), dashboard.recv()).await
            {
                return event;
            }
        }
    })
    .await
    .expect("the event crosses instances");

    assert_eq!(received, event());
    assert_eq!(
        local.recv().await.unwrap(),
        event(),
        "delivered locally too"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn wakes_and_domain_changes_are_applied() {
    let wake = Arc::new(Notify::new());
    let reloads = Arc::new(AtomicUsize::new(0));
    let counter = reloads.clone();
    let handlers = Handlers {
        events: None,
        wake: Some(wake.clone()),
        domains_changed: Some(Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        })),
    };

    uptime_runtime::apply_bus_message(BusMessage::Wake, &handlers).await;
    uptime_runtime::apply_bus_message(BusMessage::DomainsChanged, &handlers).await;

    tokio::time::timeout(Duration::from_secs(1), wake.notified())
        .await
        .expect("the scheduler is woken");
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
}
