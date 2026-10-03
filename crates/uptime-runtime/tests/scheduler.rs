#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The scheduler against a real database, with scripted and real probes.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use jiff::{SignedDuration, Timestamp, Unit};
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;
use uptime_domain::{
    CheckPolicy, CheckSpec, FailureKind, Health, HttpCheck, MonitorId, MonitorSpec, MonitorState,
    Observation, Transition,
};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};
use uptime_runtime::{EventBus, Probe, Scheduler, SchedulerConfig};
use uptime_store::Store;
use uptime_testkit::TestDb;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

// ── Test doubles ─────────────────────────────────────────────────────────

/// Returns scripted observations in order, then `fallback`; tracks concurrency.
struct ScriptedProbe {
    script: Mutex<VecDeque<Observation>>,
    fallback: Observation,
    delay: Duration,
    running: AtomicUsize,
    max_running: AtomicUsize,
    calls: AtomicUsize,
}

impl ScriptedProbe {
    fn new(script: Vec<Observation>, fallback: Observation) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            fallback,
            delay: Duration::ZERO,
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        })
    }

    fn slow(fallback: Observation, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            ..Arc::into_inner(Self::new(vec![], fallback)).unwrap()
        })
    }
}

impl Probe for ScriptedProbe {
    async fn probe(&self, _check: &CheckSpec, _timeout: Duration) -> Observation {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(running, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        let next = self.script.lock().unwrap().pop_front();
        next.unwrap_or_else(|| self.fallback.clone())
    }
}

fn ok(latency_ms: u64) -> Observation {
    Observation::Responded {
        latency: Duration::from_millis(latency_ms),
        status_code: Some(200),
        keyword_found: None,
        json_matched: None,
        cert_expires_at: None,
        response_body: None,
    }
}

fn refused() -> Observation {
    Observation::Failed {
        kind: FailureKind::Refused,
        message: "connection refused".into(),
    }
}

/// A clock the test moves by hand.
#[derive(Clone)]
struct ManualClock(Arc<Mutex<Timestamp>>);

impl ManualClock {
    fn at(now: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }
    fn set(&self, now: Timestamp) {
        *self.0.lock().unwrap() = now;
    }
    fn get(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
    fn as_clock(&self) -> uptime_runtime::Clock {
        let this = self.clone();
        Arc::new(move || this.get())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn base_time() -> Timestamp {
    Timestamp::now().round(Unit::Second).unwrap()
}

fn spec(key: &str, retries: u32) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: key.into(),
        check: CheckSpec::Http(HttpCheck::get("https://example.com/".parse().unwrap())),
        policy: CheckPolicy {
            interval: Duration::from_secs(60),
            retry_interval: Duration::from_secs(20),
            retries,
            ..Default::default()
        },
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

fn config(concurrency: usize) -> SchedulerConfig {
    SchedulerConfig {
        claimer: "test".into(),
        region: "test-region".into(),
        concurrency,
        poll_interval: Duration::from_millis(50),
        ..SchedulerConfig::default()
    }
}

fn scheduler<P: Probe>(
    store: &Store,
    probe: Arc<P>,
    clock: &ManualClock,
    concurrency: usize,
) -> Scheduler<P> {
    Scheduler::new(
        store.clone(),
        probe,
        EventBus::default(),
        config(concurrency),
    )
    .with_clock(clock.as_clock())
}

async fn state(store: &Store, id: MonitorId) -> (MonitorState, u32, Timestamp) {
    let snapshot = store.runtime(id).await.unwrap().unwrap();
    (
        snapshot.runtime.state,
        snapshot.runtime.consecutive_failures,
        snapshot.next_run_at,
    )
}

fn plus(t: Timestamp, seconds: i64) -> Timestamp {
    t + SignedDuration::from_secs(seconds)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_due_monitor_is_checked_and_recorded() {
    let db = TestDb::new().await;
    let start = base_time();
    let monitor = db
        .store()
        .create_monitor(&spec("api", 0), start)
        .await
        .unwrap();
    let clock = ManualClock::at(plus(start, 2));

    let ran = scheduler(db.store(), ScriptedProbe::new(vec![], ok(42)), &clock, 4)
        .run_due()
        .await
        .unwrap();

    assert_eq!(ran, 1);
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Up, 0, plus(start, 60))
    );
    let history = db.store().recent_checks(monitor.id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].health, Health::Up);
    assert_eq!(history[0].latency_ms, Some(42));
    assert_eq!(history[0].region, "test-region");
    assert_eq!(history[0].scheduled_for, start);
    assert_eq!(history[0].checked_at, plus(start, 2));
}

#[tokio::test]
async fn monitors_that_are_not_due_are_left_alone() {
    let db = TestDb::new().await;
    let start = base_time();
    db.store()
        .create_monitor(&spec("later", 0), plus(start, 30))
        .await
        .unwrap();
    let probe = ScriptedProbe::new(vec![], ok(1));

    let ran = scheduler(db.store(), probe.clone(), &ManualClock::at(start), 4)
        .run_due()
        .await
        .unwrap();

    assert_eq!(ran, 0);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failures_retry_then_go_down_then_recover() {
    let db = TestDb::new().await;
    let start = base_time();
    let monitor = db
        .store()
        .create_monitor(&spec("flaky", 2), start)
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let probe = ScriptedProbe::new(
        vec![refused(), refused(), refused(), refused(), ok(5)],
        ok(5),
    );
    let scheduler = scheduler(db.store(), probe, &clock, 4);
    let mut events = scheduler.events().subscribe();

    // Two tolerated failures at the retry cadence (20s)...
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Pending, 1, plus(start, 20))
    );
    assert!(db.store().list_incidents(10).await.unwrap().is_empty());
    clock.set(plus(start, 20));
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Pending, 2, plus(start, 40))
    );
    assert!(db.store().list_incidents(10).await.unwrap().is_empty());

    // The original check and both retries failed: DOWN, still checking every 20s.
    clock.set(plus(start, 40));
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Down, 3, plus(start, 60))
    );
    let incident = db.store().list_incidents(10).await.unwrap().remove(0);
    assert_eq!(incident.started_at, plus(start, 40));
    assert_eq!(incident.updates.len(), 4);

    clock.set(plus(start, 60));
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Down, 4, plus(start, 80))
    );
    assert_eq!(db.store().list_incidents(10).await.unwrap().len(), 1);

    // Recovery resolves the incident and restores the normal interval (60s).
    clock.set(plus(start, 80));
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(db.store(), monitor.id).await,
        (MonitorState::Up, 0, plus(start, 140))
    );
    assert_eq!(
        db.store()
            .incident(incident.id)
            .await
            .unwrap()
            .unwrap()
            .resolved_at,
        Some(plus(start, 80))
    );

    let mut transitions = Vec::new();
    while let Ok(event) = events.try_recv() {
        assert_eq!(event.monitor_id, monitor.id);
        transitions.extend(event.transition);
    }
    assert_eq!(transitions, [Transition::WentDown, Transition::Recovered]);
}

#[tokio::test]
async fn failed_checks_are_logged_with_their_monitor_and_reason() {
    let db = TestDb::new().await;
    let start = base_time();
    db.store()
        .create_monitor(&spec("flaky", 1), start)
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let probe = ScriptedProbe::new(vec![refused(), refused(), ok(5)], ok(5));
    let scheduler = scheduler(db.store(), probe, &clock, 4);
    let (logs, _guard) = uptime_testkit::logs::Logs::capture("info");

    for at in [0, 20, 80, 140] {
        clock.set(plus(start, at));
        scheduler.run_due().await.unwrap();
    }

    let messages: Vec<String> = logs
        .lines()
        .iter()
        .map(|line| line["message"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        messages,
        ["check failed", "monitor down", "monitor recovered"],
        "passing checks stay out of the info log"
    );
    let failed = &logs.with_message("check failed")[0];
    assert_eq!(failed["level"], "WARN");
    assert_eq!(failed["span"]["key"], "flaky");
    assert_eq!(failed["reason"], "refused: connection refused");
    assert_eq!(failed["state"], "pending");
    assert_eq!(failed["failures"], 1);
    let down = &logs.with_message("monitor down")[0];
    assert_eq!(down["level"], "WARN");
    assert_eq!(down["state"], "down");
    let recovered = &logs.with_message("monitor recovered")[0];
    assert_eq!(recovered["level"], "INFO");
    assert_eq!(recovered["latency_ms"], 5);
}

#[tokio::test]
async fn maintenance_windows_silence_failures() {
    let db = TestDb::new().await;
    let start = base_time();
    let store = db.store();
    let monitor = store.create_monitor(&spec("db", 0), start).await.unwrap();
    store
        .create_maintenance(
            &uptime_domain::MaintenanceSpec {
                title: "Upgrade".into(),
                description: None,
                starts_at: start,
                ends_at: plus(start, 90),
                repeat: None,
                repeat_until: None,
                monitors: vec!["db".parse().unwrap()],
            },
            start,
        )
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let scheduler = scheduler(store, ScriptedProbe::new(vec![], refused()), &clock, 4);
    let mut events = scheduler.events().subscribe();

    scheduler.run_due().await.unwrap();
    assert_eq!(state(store, monitor.id).await.0, MonitorState::Maintenance);
    assert_eq!(
        events.try_recv().unwrap().transition,
        None,
        "no alert during maintenance"
    );

    // The window ends; the failure now counts.
    clock.set(plus(start, 90));
    scheduler.run_due().await.unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(state(store, monitor.id).await.0, MonitorState::Down);
    assert_eq!(event.transition, Some(Transition::WentDown));
}

#[tokio::test]
async fn push_monitors_are_judged_by_their_heartbeats() {
    let db = TestDb::new().await;
    let start = base_time();
    let store = db.store();
    let monitor = store
        .create_monitor(
            &MonitorSpec {
                check: CheckSpec::Push(uptime_domain::PushCheck {
                    token: "tokentokentokentoken".into(),
                }),
                ..spec("cron", 0)
            },
            plus(start, 3600),
        )
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let probe = ScriptedProbe::new(vec![], ok(1));
    let scheduler = scheduler(store, probe.clone(), &clock, 4);

    // A heartbeat makes the monitor due at once and UP.
    store
        .record_push(
            "tokentokentokentoken",
            &uptime_domain::Push {
                at: start,
                up: true,
                message: None,
                ping: None,
            },
        )
        .await
        .unwrap();
    scheduler.run_due().await.unwrap();
    assert_eq!(state(store, monitor.id).await.0, MonitorState::Up);
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "push monitors are never probed"
    );

    // Silence past interval + grace: DOWN.
    clock.set(plus(start, 60));
    scheduler.run_due().await.unwrap();
    assert_eq!(
        state(store, monitor.id).await.0,
        MonitorState::Up,
        "still within the grace"
    );
    clock.set(plus(start, 120));
    scheduler.run_due().await.unwrap();
    assert_eq!(state(store, monitor.id).await.0, MonitorState::Down);
    let last = store.recent_checks(monitor.id, 1).await.unwrap().remove(0);
    assert!(last.error.unwrap().contains("no push"), "a clear reason");
}

#[tokio::test]
async fn expiring_certificates_alert_once_per_threshold() {
    let db = TestDb::new().await;
    let start = base_time();
    let store = db.store();
    let monitor = store.create_monitor(&spec("site", 0), start).await.unwrap();
    let channel = store
        .create_channel(
            &uptime_domain::ChannelSpec {
                name: "Ops".into(),
                kind: uptime_domain::ChannelKind::Webhook,
                url: "https://ops.example.com/hook".parse().unwrap(),
                secret: None,
                default_on: false,
                routing: Default::default(),
            },
            start,
        )
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[channel.id])
        .await
        .unwrap();
    let expiring = Observation::Responded {
        latency: Duration::from_millis(5),
        status_code: Some(200),
        keyword_found: None,
        json_matched: None,
        cert_expires_at: Some(plus(start, 10 * 86_400 + 3600)),
        response_body: None,
    };
    let clock = ManualClock::at(start);
    let scheduler = scheduler(store, ScriptedProbe::new(vec![], expiring), &clock, 4);

    scheduler.run_due().await.unwrap();
    clock.set(plus(start, 60));
    scheduler.run_due().await.unwrap();

    let deliveries = store.recent_deliveries(10).await.unwrap();
    assert_eq!(deliveries.len(), 1, "one warning for the 14-day threshold");
    assert_eq!(
        deliveries[0].alert.event,
        uptime_domain::AlertEvent::CertExpiring
    );
    assert_eq!(deliveries[0].alert.cert_days_left, Some(10));
    let runtime = store.runtime(monitor.id).await.unwrap().unwrap();
    assert_eq!(
        runtime.cert_expires_at,
        Some(plus(start, 10 * 86_400 + 3600))
    );
}

#[tokio::test]
async fn concurrency_limits_checks_in_flight() {
    let db = TestDb::new().await;
    let start = base_time();
    for i in 0..7 {
        db.store()
            .create_monitor(&spec(&format!("m{i}"), 0), start)
            .await
            .unwrap();
    }
    let probe = ScriptedProbe::slow(ok(1), Duration::from_millis(50));
    let scheduler = scheduler(db.store(), probe.clone(), &ManualClock::at(start), 3);

    let batches = [
        scheduler.run_due().await.unwrap(),
        scheduler.run_due().await.unwrap(),
        scheduler.run_due().await.unwrap(),
        scheduler.run_due().await.unwrap(),
    ];

    assert_eq!(batches, [3, 3, 1, 0]);
    assert_eq!(probe.max_running.load(Ordering::SeqCst), 3);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn the_loop_runs_checks_until_shutdown() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&spec("loop", 0), Timestamp::now())
        .await
        .unwrap();
    let scheduler = Scheduler::new(
        db.store().clone(),
        ScriptedProbe::new(vec![], ok(3)),
        EventBus::default(),
        config(4),
    );
    let mut events = scheduler.events().subscribe();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(scheduler.run(shutdown.clone()));

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a check ran")
        .unwrap();
    assert_eq!(event.monitor_id, monitor.id);
    assert_eq!(event.state, MonitorState::Up);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("loop stops")
        .unwrap();
}

#[tokio::test]
async fn the_loop_pings_the_heartbeat() {
    let db = TestDb::new().await;
    let heartbeat = MockServer::start().await;
    Mock::given(path("/ping"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&heartbeat)
        .await;
    let config = SchedulerConfig {
        heartbeat_url: Some(format!("{}/ping", heartbeat.uri()).parse().unwrap()),
        ..config(4)
    };
    let scheduler = Scheduler::new(
        db.store().clone(),
        ScriptedProbe::new(vec![], ok(1)),
        EventBus::default(),
        config,
    );
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(scheduler.run(shutdown.clone()));

    tokio::time::timeout(Duration::from_secs(5), async {
        while heartbeat.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("heartbeat pinged");

    shutdown.cancel();
    running.await.unwrap();
}

#[tokio::test]
async fn end_to_end_with_the_real_prober() {
    let db = TestDb::new().await;
    let target = MockServer::start().await;
    Mock::given(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&target)
        .await;
    let spec = MonitorSpec {
        check: CheckSpec::Http(HttpCheck::get(
            format!("{}/health", target.uri()).parse().unwrap(),
        )),
        ..spec("real", 0)
    };
    let start = base_time();
    let monitor = db.store().create_monitor(&spec, start).await.unwrap();
    let prober = Prober::new(ProberConfig {
        policy: AddressPolicy::ALLOW_ALL,
        ..ProberConfig::default()
    })
    .unwrap();

    let ran = scheduler(db.store(), Arc::new(prober), &ManualClock::at(start), 4)
        .run_due()
        .await
        .unwrap();

    assert_eq!(ran, 1);
    let history = db.store().recent_checks(monitor.id, 1).await.unwrap();
    assert_eq!(history[0].status_code, Some(200));
    assert_eq!(history[0].state_after, MonitorState::Up);
}

#[tokio::test]
async fn incidents_keep_original_retry_and_later_failures_until_recovery() {
    let db = TestDb::new().await;
    let store = db.store();
    let start = base_time();
    let monitor = store
        .create_monitor(&spec("incident", 1), start)
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let failure = |status, body: &str| Observation::Responded {
        status_code: Some(status),
        response_body: Some(body.into()),
        latency: Duration::from_millis(7),
        keyword_found: None,
        json_matched: None,
        cert_expires_at: None,
    };
    let runner = scheduler(
        store,
        ScriptedProbe::new(
            vec![
                failure(503, "first failure"),
                failure(502, "retry failure"),
                refused(),
                ok(5),
                failure(500, "new outage"),
                failure(500, "new retry"),
            ],
            ok(5),
        ),
        &clock,
        4,
    );

    runner.run_due().await.unwrap();
    assert!(store.list_incidents(10).await.unwrap().is_empty());
    clock.set(plus(start, 20));
    runner.run_due().await.unwrap();
    let open = store.list_incidents(10).await.unwrap().remove(0);
    assert_eq!(open.status, uptime_domain::IncidentStatus::Investigating);
    assert_eq!(open.started_at, plus(start, 20));
    assert_eq!(open.updates.len(), 3);
    let retry = open.updates[0].check.as_ref().unwrap();
    assert_eq!(retry.status_code, Some(502));
    assert_eq!(retry.response_body.as_deref(), Some("retry failure"));
    assert_eq!(retry.error.as_deref(), Some("unexpected status 502"));
    assert_eq!(retry.state_after, MonitorState::Down);
    let original = open.updates[2].check.as_ref().unwrap();
    assert_eq!(original.status_code, Some(503));
    assert_eq!(original.response_body.as_deref(), Some("first failure"));
    assert_eq!(original.state_after, MonitorState::Pending);
    assert_eq!(original.region, "test-region");

    clock.set(plus(start, 80));
    runner.run_due().await.unwrap();
    assert_eq!(store.list_incidents(10).await.unwrap().len(), 1);
    let still_open = store.incident(open.id).await.unwrap().unwrap();
    assert_eq!(still_open.updates.len(), 4);
    assert_eq!(
        still_open.updates[0].check.as_ref().unwrap().error_kind,
        Some(FailureKind::Refused)
    );

    // A maintenance check hides the Down state; recovery must still resolve the incident.
    store
        .create_maintenance(
            &uptime_domain::MaintenanceSpec {
                title: "Maintenance".into(),
                description: None,
                starts_at: plus(start, 100),
                ends_at: plus(start, 150),
                repeat: None,
                repeat_until: None,
                monitors: vec!["incident".parse().unwrap()],
            },
            start,
        )
        .await
        .unwrap();
    clock.set(plus(start, 140));
    runner.run_due().await.unwrap();
    assert_eq!(state(store, monitor.id).await.0, MonitorState::Maintenance);
    assert_eq!(
        store.incident(open.id).await.unwrap().unwrap().resolved_at,
        None
    );
    // Failures after maintenance belong to the same open incident.
    clock.set(plus(start, 200));
    runner.run_due().await.unwrap();
    clock.set(plus(start, 220));
    runner.run_due().await.unwrap();
    // Both failed again, so the existing incident stays open.
    assert_eq!(store.list_incidents(10).await.unwrap().len(), 1);
    clock.set(plus(start, 280));
    runner.run_due().await.unwrap();
    let resolved = store.incident(open.id).await.unwrap().unwrap();
    assert_eq!(resolved.status, uptime_domain::IncidentStatus::Resolved);
    assert_eq!(resolved.resolved_at, Some(plus(start, 280)));
    assert_eq!(resolved.updates[0].body, "incident recovered after 4m 20s.");
    assert_eq!(resolved.updates.len(), 7);
    store
        .prune_checks_before(plus(start, 300), 10)
        .await
        .unwrap();
    assert_eq!(store.incident(open.id).await.unwrap().unwrap(), resolved);

    // A new confirmed outage gets a fresh incident.
    for at in [340, 360] {
        // Scripted failures are exhausted; use another scheduler to fail twice.
        clock.set(plus(start, at));
        scheduler(store, ScriptedProbe::new(vec![], refused()), &clock, 4)
            .run_due()
            .await
            .unwrap();
    }
    assert_eq!(store.list_incidents(10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_successful_retry_does_not_open_an_incident() {
    let db = TestDb::new().await;
    let start = base_time();
    let monitor = db
        .store()
        .create_monitor(&spec("transient", 2), start)
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let scheduler = scheduler(
        db.store(),
        ScriptedProbe::new(vec![refused(), refused(), ok(1)], ok(1)),
        &clock,
        4,
    );
    scheduler.run_due().await.unwrap();
    clock.set(plus(start, 20));
    scheduler.run_due().await.unwrap();
    assert_eq!(state(db.store(), monitor.id).await.0, MonitorState::Pending);
    assert!(db.store().list_incidents(10).await.unwrap().is_empty());
    clock.set(plus(start, 40));
    scheduler.run_due().await.unwrap();
    assert_eq!(state(db.store(), monitor.id).await.0, MonitorState::Up);
    assert!(db.store().list_incidents(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn incidents_require_two_failures_even_when_retries_are_disabled() {
    let db = TestDb::new().await;
    let start = base_time();
    db.store()
        .create_monitor(&spec("no-retries", 0), start)
        .await
        .unwrap();
    let clock = ManualClock::at(start);
    let scheduler = scheduler(db.store(), ScriptedProbe::new(vec![], refused()), &clock, 4);
    scheduler.run_due().await.unwrap();
    assert!(db.store().list_incidents(10).await.unwrap().is_empty());
    clock.set(plus(start, 20));
    scheduler.run_due().await.unwrap();
    let incidents = db.store().list_incidents(10).await.unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].updates.len(), 3);
}
