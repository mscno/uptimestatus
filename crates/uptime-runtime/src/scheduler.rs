//! The check scheduler: claim due slots from Postgres, probe, evaluate, record.
//!
//! Claiming goes through `FOR UPDATE SKIP LOCKED` (see `uptime-store`), so any
//! number of schedulers can run side by side without executing a slot twice.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use jiff::{Timestamp, Unit};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use uptime_domain::{CheckSpec, Health, Observation, Report, Transition, evaluate, state};
use uptime_store::{CertUpdate, CheckRecord, Claim, Store, StoreError};
use url::Url;

use crate::{CheckCompleted, EventBus, Probe, schedule};

/// Source of "now". Rounded to microseconds, the precision Postgres stores.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        let now = Timestamp::now();
        now.round(Unit::Microsecond).unwrap_or(now)
    })
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Recorded as `claimed_by`; the instance's host name in production.
    pub claimer: String,
    /// Recorded with every result; where the instance runs (`checks.region`).
    pub region: String,
    /// Checks in flight at once.
    pub concurrency: usize,
    /// How often the loop looks for due checks when nothing wakes it.
    pub poll_interval: Duration,
    /// How long shutdown waits for in-flight checks.
    pub drain_timeout: Duration,
    /// Dead-man switch pinged (at most once per `heartbeat_every`) while the loop is healthy.
    pub heartbeat_url: Option<Url>,
    pub heartbeat_every: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            claimer: "local".into(),
            region: "local".into(),
            concurrency: 32,
            poll_interval: Duration::from_secs(1),
            drain_timeout: Duration::from_secs(10),
            heartbeat_url: None,
            heartbeat_every: Duration::from_secs(60),
        }
    }
}

/// Runs checks. Cheap to clone; clones share the concurrency limit.
pub struct Scheduler<P> {
    inner: Arc<Inner<P>>,
}

struct Inner<P> {
    store: Store,
    probe: Arc<P>,
    events: EventBus,
    config: SchedulerConfig,
    clock: Clock,
    permits: Arc<Semaphore>,
    wake: Arc<Notify>,
    heartbeat: Heartbeat,
}

impl<P> Clone for Scheduler<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<P: Probe> Scheduler<P> {
    pub fn new(store: Store, probe: Arc<P>, events: EventBus, config: SchedulerConfig) -> Self {
        Self::build(store, probe, events, config, system_clock())
    }

    fn build(
        store: Store,
        probe: Arc<P>,
        events: EventBus,
        config: SchedulerConfig,
        clock: Clock,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.concurrency.max(1)));
        let heartbeat = Heartbeat::new(config.heartbeat_url.clone(), config.heartbeat_every);
        Self {
            inner: Arc::new(Inner {
                store,
                probe,
                events,
                config,
                clock,
                permits,
                wake: Arc::default(),
                heartbeat,
            }),
        }
    }

    /// Replaces the clock (tests). Call before sharing the scheduler.
    pub fn with_clock(self, clock: Clock) -> Self {
        let inner = Arc::try_unwrap(self.inner)
            .unwrap_or_else(|_| panic!("with_clock after the scheduler was shared"));
        Self::build(inner.store, inner.probe, inner.events, inner.config, clock)
    }

    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// Wakes the loop immediately (e.g. after a monitor was created or edited).
    pub fn waker(&self) -> Arc<Notify> {
        self.inner.wake.clone()
    }

    /// Claims due checks (up to the free concurrency) and runs them to completion.
    /// Returns how many ran.
    pub async fn run_due(&self) -> Result<usize, StoreError> {
        let mut running = JoinSet::new();
        let started = self.spawn_due(&mut running).await?;
        while running.join_next().await.is_some() {}
        Ok(started)
    }

    /// Runs until `shutdown` is cancelled, then waits (bounded) for in-flight checks.
    pub async fn run(self, shutdown: CancellationToken) {
        let config = &self.inner.config;
        tracing::info!(concurrency = config.concurrency, region = %config.region, "scheduler started");
        let mut running = JoinSet::new();
        loop {
            match self.spawn_due(&mut running).await {
                Ok(_) => self.inner.heartbeat.beat(),
                Err(error) => {
                    tracing::error!(error = %Report(&error), "claiming due checks failed")
                }
            }
            while running.try_join_next().is_some() {}
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(config.poll_interval) => {}
                () = self.inner.wake.notified() => {}
            }
        }

        tracing::info!(in_flight = running.len(), "scheduler draining");
        let drain = async { while running.join_next().await.is_some() {} };
        if tokio::time::timeout(config.drain_timeout, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                "in-flight checks did not finish in time; their slots will be reclaimed"
            );
            running.abort_all();
        }
        tracing::info!("scheduler stopped");
    }

    /// Claims as many due checks as there are free permits and spawns them.
    async fn spawn_due(&self, running: &mut JoinSet<()>) -> Result<usize, StoreError> {
        let free = self.inner.permits.available_permits();
        if free == 0 {
            return Ok(0);
        }
        let now = (self.inner.clock)();
        let limit = u32::try_from(free).unwrap_or(u32::MAX);
        let claims = self
            .inner
            .store
            .claim_due(now, limit, &self.inner.config.claimer)
            .await?;
        let started = claims.len();
        let maintenance = if claims.is_empty() {
            Default::default()
        } else {
            self.inner.store.monitors_in_maintenance(now).await?
        };
        for claim in claims {
            let in_maintenance = maintenance.contains(&claim.monitor.id);
            let Ok(permit) = self.inner.permits.clone().try_acquire_owned() else {
                // Cannot happen: we claimed at most `free`. The slot's watchdog reclaims it.
                tracing::error!(monitor = %claim.monitor.id, "no permit for a claimed check");
                continue;
            };
            let inner = self.inner.clone();
            running.spawn(async move { inner.execute(claim, permit, in_maintenance).await });
        }
        Ok(started)
    }
}

/// One line per check that did not simply pass (passing checks log at
/// DEBUG): failures with their reason, and the monitor going down or
/// recovering. The monitor is in the enclosing span. Failures inside a
/// maintenance window are expected, so they log at INFO.
fn log_outcome(record: &CheckRecord, in_maintenance: bool) {
    let verdict = &record.verdict;
    let reason = verdict.reason.as_ref().map(ToString::to_string);
    let latency_ms = verdict
        .latency
        .map(|l| u64::try_from(l.as_millis()).unwrap_or(u64::MAX));
    let state = record.runtime.state.as_str();
    let failures = record.runtime.consecutive_failures;
    let message = match (verdict.health, record.transition) {
        (_, Some(Transition::Recovered)) => {
            tracing::info!(latency_ms, state, "monitor recovered");
            return;
        }
        (Health::Up, _) => {
            tracing::debug!(latency_ms, "check passed");
            return;
        }
        (Health::Degraded, _) => "check slow",
        (Health::Down, Some(Transition::WentDown)) => "monitor down",
        (Health::Down, Some(Transition::Resend)) => "monitor still down",
        (Health::Down, None) => "check failed",
    };
    let status_code = verdict.status_code;
    if in_maintenance {
        tracing::info!(
            reason,
            status_code,
            latency_ms,
            state,
            failures,
            in_maintenance,
            "{message}"
        );
    } else {
        tracing::warn!(
            reason,
            status_code,
            latency_ms,
            state,
            failures,
            "{message}"
        );
    }
}

impl<P: Probe> Inner<P> {
    /// Probes, evaluates, advances the state machine and records the result.
    #[tracing::instrument(skip_all, fields(monitor = %claim.monitor.id, key = %claim.monitor.spec.key, check = claim.monitor.spec.check.kind()))]
    async fn execute(&self, claim: Claim, _permit: OwnedSemaphorePermit, in_maintenance: bool) {
        let spec = &claim.monitor.spec;
        let observation = match &spec.check {
            CheckSpec::Push(_) => uptime_domain::push::observe(
                claim.last_push.as_ref(),
                (self.clock)(),
                spec.policy.interval,
            ),
            check => self.probe.probe(check, spec.policy.timeout).await,
        };
        let checked_at = (self.clock)();
        let verdict = evaluate(&spec.check, &spec.policy, &observation);
        let step = state::apply(claim.runtime, &spec.policy, verdict.health, in_maintenance);
        let cert = cert_update(
            &spec.check,
            &observation,
            claim.cert_warned_days,
            checked_at,
            in_maintenance,
        );
        let record = CheckRecord {
            monitor_id: claim.monitor.id,
            scheduled_for: claim.scheduled_for,
            checked_at,
            verdict,
            runtime: step.runtime,
            transition: step.transition,
            cert,
            next_run_at: schedule::next_run_at(claim.scheduled_for, step.next_in, checked_at),
            region: self.config.region.clone(),
        };

        log_outcome(&record, in_maintenance);
        if let Err(error) = self.store.record_check(&record).await {
            tracing::error!(error = %Report(&error), "recording the check failed; the slot will be retried");
            return;
        }

        self.events.publish(CheckCompleted {
            monitor_id: claim.monitor.id,
            key: spec.key.clone(),
            previous: claim.runtime.state,
            state: step.runtime.state,
            transition: step.transition,
            latency_ms: record
                .verdict
                .latency
                .map(|l| i64::try_from(l.as_millis()).unwrap_or(i64::MAX)),
            checked_at,
        });
    }
}

/// The certificate an HTTPS check saw, and whether it is time to warn about
/// it (never during maintenance).
fn cert_update(
    check: &CheckSpec,
    observation: &Observation,
    warned: Option<u32>,
    now: Timestamp,
    in_maintenance: bool,
) -> Option<CertUpdate> {
    let warn_days = match check {
        CheckSpec::Http(http) => http.cert_expiry_warn_days,
        CheckSpec::Tcp(tcp) if tcp.tls => tcp.cert_expiry_warn_days,
        _ => return None,
    };
    let Observation::Responded {
        cert_expires_at: Some(expires_at),
        ..
    } = observation
    else {
        return None;
    };
    let (warned_days, alert) = uptime_domain::cert::warning(*expires_at, now, warn_days, warned);
    Some(CertUpdate {
        expires_at: *expires_at,
        warned_days,
        alert_days_left: (alert && !in_maintenance)
            .then(|| uptime_domain::cert::days_left(*expires_at, now)),
    })
}

/// Pings a dead-man switch (e.g. healthchecks.io) while the loop is healthy,
/// so a hung scheduler pages someone even when HTTP still answers.
struct Heartbeat {
    url: Option<Url>,
    every: Duration,
    client: reqwest::Client,
    last: Mutex<Option<Instant>>,
}

impl Heartbeat {
    fn new(url: Option<Url>, every: Duration) -> Self {
        Self {
            url,
            every,
            client: reqwest::Client::new(),
            last: Mutex::new(None),
        }
    }

    /// Fires a ping in the background if one is due. Never blocks the loop.
    fn beat(&self) {
        let Some(url) = self.url.clone() else { return };
        {
            let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|at| at.elapsed() < self.every) {
                return;
            }
            *last = Some(Instant::now());
        }
        let request = self.client.get(url).timeout(Duration::from_secs(10));
        tokio::spawn(async move {
            if let Err(error) = request
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                tracing::warn!(error = %Report(&error), "heartbeat ping failed");
            }
        });
    }
}
