#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The notification sender against a real outbox and a scripted channel.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;
use uptime_domain::{
    Alert, ChannelKind, ChannelSpec, CheckSpec, Health, MonitorId, MonitorSpec, MonitorState,
    Runtime, TcpCheck, Transition, Verdict,
};
use uptime_notify::DeliveryError;
use uptime_runtime::{
    CheckCompleted, Deliver, EventBus, Notifier, NotifierConfig, notifier::backoff,
};
use uptime_store::{CheckRecord, DeliveryStatus, Store};
use uptime_testkit::TestDb;

/// Answers deliveries from a script (default: success) and records them.
#[derive(Clone, Default)]
struct FakeChannel {
    script: Arc<Mutex<VecDeque<Result<(), DeliveryError>>>>,
    sent: Arc<Mutex<Vec<(String, Alert)>>>,
}

impl FakeChannel {
    fn then(&self, result: Result<(), DeliveryError>) -> &Self {
        self.script.lock().unwrap().push_back(result);
        self
    }

    fn sent(&self) -> Vec<(String, Alert)> {
        self.sent.lock().unwrap().clone()
    }
}

impl Deliver for FakeChannel {
    fn deliver(
        &self,
        channel: &ChannelSpec,
        alert: &Alert,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send {
        self.sent
            .lock()
            .unwrap()
            .push((channel.name.clone(), alert.clone()));
        let result = self.script.lock().unwrap().pop_front().unwrap_or(Ok(()));
        async move { result }
    }
}

fn transient(message: &str) -> DeliveryError {
    DeliveryError {
        message: message.into(),
        permanent: false,
        retry_after: None,
    }
}

fn base() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

fn at(seconds: i64) -> Timestamp {
    base() + SignedDuration::from_secs(seconds)
}

/// A clock the test moves by hand.
#[derive(Clone)]
struct Clock(Arc<Mutex<Timestamp>>);

impl Clock {
    fn set(&self, seconds: i64) {
        *self.0.lock().unwrap() = at(seconds);
    }
}

struct Setup {
    db: TestDb,
    channel: FakeChannel,
    clock: Clock,
    notifier: Notifier<FakeChannel>,
    monitor: MonitorId,
}

async fn setup() -> Setup {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store
        .create_monitor(
            &MonitorSpec {
                key: "db".parse().unwrap(),
                name: "Database".into(),
                check: CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432)),
                policy: Default::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            base(),
        )
        .await
        .unwrap()
        .id;
    let ops = store
        .create_channel(
            &ChannelSpec {
                name: "Ops".into(),
                kind: ChannelKind::Webhook,
                url: "https://ops.example.com/hook".parse().unwrap(),
                secret: None,
                default_on: false,
                routing: Default::default(),
            },
            base(),
        )
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor, &[ops.id])
        .await
        .unwrap();
    let channel = FakeChannel::default();
    let clock = Clock(Arc::new(Mutex::new(base())));
    let tick = clock.clone();
    let notifier = Notifier::new(
        store.clone(),
        Arc::new(channel.clone()),
        NotifierConfig::default(),
    )
    .with_clock(Arc::new(move || *tick.0.lock().unwrap()));
    Setup {
        db,
        channel,
        clock,
        notifier,
        monitor,
    }
}

async fn went_down(store: &Store, monitor: MonitorId, seconds: i64) {
    store
        .record_check(&CheckRecord {
            monitor_id: monitor,
            scheduled_for: at(seconds),
            checked_at: at(seconds),
            verdict: Verdict {
                health: Health::Down,
                latency: None,
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Down,
                consecutive_failures: 1,
            },
            transition: Some(Transition::WentDown),
            cert: None,
            next_run_at: at(seconds + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

async fn status(store: &Store) -> (DeliveryStatus, u32) {
    let log = store.recent_deliveries(1).await.unwrap().remove(0);
    (log.status, log.attempts)
}

#[tokio::test]
async fn queued_alerts_are_delivered_once() {
    let s = setup().await;
    went_down(s.db.store(), s.monitor, 0).await;

    assert_eq!(s.notifier.run_once().await.unwrap(), 1);
    assert_eq!(s.notifier.run_once().await.unwrap(), 0);

    let sent = s.channel.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "Ops");
    assert_eq!(sent[0].1.monitor.name, "Database");
    assert_eq!(status(s.db.store()).await, (DeliveryStatus::Sent, 1));
}

#[tokio::test]
async fn transient_failures_are_retried_with_backoff() {
    let s = setup().await;
    s.channel
        .then(Err(transient("502")))
        .then(Err(transient("502")));
    went_down(s.db.store(), s.monitor, 0).await;

    s.notifier.run_once().await.unwrap();
    assert_eq!(status(s.db.store()).await, (DeliveryStatus::Pending, 1));

    s.clock.set(59);
    assert_eq!(
        s.notifier.run_once().await.unwrap(),
        0,
        "not before 1 minute"
    );
    s.clock.set(60);
    assert_eq!(s.notifier.run_once().await.unwrap(), 1);

    s.clock.set(60 + 299);
    assert_eq!(s.notifier.run_once().await.unwrap(), 0, "then 5 minutes");
    s.clock.set(60 + 300);
    assert_eq!(s.notifier.run_once().await.unwrap(), 1);

    assert_eq!(status(s.db.store()).await, (DeliveryStatus::Sent, 3));
}

#[tokio::test]
async fn receivers_can_ask_for_a_longer_wait() {
    let s = setup().await;
    s.channel.then(Err(DeliveryError {
        retry_after: Some(Duration::from_secs(600)),
        ..transient("429")
    }));
    went_down(s.db.store(), s.monitor, 0).await;
    s.notifier.run_once().await.unwrap();

    s.clock.set(599);
    assert_eq!(s.notifier.run_once().await.unwrap(), 0);
    s.clock.set(600);
    assert_eq!(s.notifier.run_once().await.unwrap(), 1);
}

#[tokio::test]
async fn permanent_failures_give_up_at_once() {
    let s = setup().await;
    s.channel.then(Err(DeliveryError {
        permanent: true,
        ..transient("404 no_service")
    }));
    went_down(s.db.store(), s.monitor, 0).await;

    s.notifier.run_once().await.unwrap();
    s.clock.set(100_000);

    assert_eq!(s.notifier.run_once().await.unwrap(), 0);
    let log = s.db.store().recent_deliveries(1).await.unwrap().remove(0);
    assert_eq!(log.status, DeliveryStatus::Failed);
    assert_eq!(log.last_error.as_deref(), Some("404 no_service"));
}

#[tokio::test]
async fn retries_stop_after_a_day() {
    let s = setup().await;
    for _ in 0..40 {
        s.channel.then(Err(transient("timeout")));
    }
    went_down(s.db.store(), s.monitor, 0).await;

    let mut now = 0;
    for _ in 0..40 {
        s.notifier.run_once().await.unwrap();
        now += 3_600;
        s.clock.set(now);
    }

    let (state, attempts) = status(s.db.store()).await;
    assert_eq!(state, DeliveryStatus::Failed);
    assert!(attempts < 30, "{attempts}");
}

#[test]
fn backoff_grows_then_caps_at_an_hour() {
    let minutes = |m: u64| Duration::from_secs(m * 60);
    assert_eq!(backoff(1), minutes(1));
    assert_eq!(backoff(2), minutes(5));
    assert_eq!(backoff(3), minutes(15));
    assert_eq!(backoff(4), minutes(60));
    assert_eq!(backoff(40), minutes(60));
}

#[tokio::test]
async fn transitions_wake_the_sender_immediately() {
    let s = setup().await;
    let events = EventBus::default();
    let channel = s.channel.clone();
    let notifier = Notifier::new(
        s.db.store().clone(),
        Arc::new(channel.clone()),
        NotifierConfig {
            poll_interval: Duration::from_secs(3600),
            ..NotifierConfig::default()
        },
    )
    .with_events(events.clone())
    .with_clock(Arc::new(|| at(1)));
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(notifier.run(shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;

    went_down(s.db.store(), s.monitor, 0).await;
    events.publish(CheckCompleted {
        monitor_id: s.monitor,
        key: "db".parse().unwrap(),
        previous: MonitorState::Up,
        state: MonitorState::Down,
        transition: Some(Transition::WentDown),
        latency_ms: None,
        checked_at: at(0),
    });

    for _ in 0..100 {
        if !channel.sent().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown.cancel();
    running.await.unwrap();
    assert_eq!(channel.sent().len(), 1);
}
