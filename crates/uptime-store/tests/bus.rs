#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! LISTEN/NOTIFY between instances, against a real Postgres.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uptime_store::bus::{BusMessage, Listener};
use uptime_testkit::TestDb;

async fn listen(
    db: &TestDb,
    instance: &str,
    shutdown: &CancellationToken,
) -> mpsc::UnboundedReceiver<BusMessage> {
    let (sink, messages) = mpsc::unbounded_channel();
    let (ready, mut is_ready) = mpsc::unbounded_channel();
    tokio::spawn(Listener::new(db.url(), instance).run(sink, Some(ready), shutdown.clone()));
    tokio::time::timeout(Duration::from_secs(10), is_ready.recv())
        .await
        .expect("listener connects")
        .unwrap();
    messages
}

async fn next(messages: &mut mpsc::UnboundedReceiver<BusMessage>) -> Option<BusMessage> {
    tokio::time::timeout(Duration::from_millis(1500), messages.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn other_instances_hear_messages_but_senders_do_not() {
    let db = TestDb::new().await;
    let shutdown = CancellationToken::new();
    let mut web = listen(&db, "web-1", &shutdown).await;
    let mut worker = listen(&db, "worker-1", &shutdown).await;

    db.store()
        .notify("worker-1", &BusMessage::DomainsChanged)
        .await
        .unwrap();

    assert_eq!(next(&mut web).await, Some(BusMessage::DomainsChanged));
    assert_eq!(next(&mut worker).await, None, "own messages are skipped");
    shutdown.cancel();
}

#[tokio::test]
async fn check_events_cross_instances_intact() {
    let db = TestDb::new().await;
    let shutdown = CancellationToken::new();
    let mut web = listen(&db, "web-1", &shutdown).await;
    let event = BusMessage::Check {
        monitor_id: 7,
        key: "api".into(),
        previous: uptime_domain::MonitorState::Up,
        state: uptime_domain::MonitorState::Down,
        transition: Some(uptime_domain::Transition::WentDown),
        latency_ms: Some(12),
        checked_at: jiff::Timestamp::from_second(1_800_000_000).unwrap(),
    };

    db.store().notify("worker-1", &event).await.unwrap();

    assert_eq!(next(&mut web).await, Some(event));
    shutdown.cancel();
}

#[tokio::test]
async fn a_listener_that_cannot_connect_retries_until_shut_down() {
    let shutdown = CancellationToken::new();
    let (sink, _messages) = mpsc::unbounded_channel();
    let listener = Listener::new("postgres://nobody@127.0.0.1:1/none?sslmode=disable", "x")
        .with_retry(Duration::from_millis(50));
    let running = tokio::spawn(listener.run(sink, None, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!running.is_finished(), "keeps retrying");
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("stops on shutdown")
        .unwrap();
}
