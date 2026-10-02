//! Delivers queued alerts: claims due outbox rows, sends them, and records the
//! outcome with backoff (1m, 5m, 15m, then hourly; gives up after a day).

use std::{future::Future, sync::Arc, time::Duration};

use jiff::{SignedDuration, Timestamp};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uptime_domain::{Alert, ChannelSpec};
use uptime_notify::DeliveryError;
use uptime_store::{Delivery, Store, StoreError};

use crate::{Clock, EventBus, system_clock};

/// Sends one alert to one channel. Implemented by [`uptime_notify::Sender`];
/// tests substitute scripted channels.
pub trait Deliver: Send + Sync + 'static {
    fn deliver(
        &self,
        channel: &ChannelSpec,
        alert: &Alert,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send;
}

impl Deliver for uptime_notify::Sender {
    fn deliver(
        &self,
        channel: &ChannelSpec,
        alert: &Alert,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send {
        self.send(channel, alert)
    }
}

#[derive(Clone, Debug)]
pub struct NotifierConfig {
    /// How often the outbox is checked when nothing wakes the sender.
    pub poll_interval: Duration,
    /// Deliveries claimed per pass.
    pub batch: u32,
    /// How long a claimed delivery is reserved before another pass may retry it.
    pub lease: Duration,
    /// Stop retrying this long after the alert was queued.
    pub give_up_after: Duration,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            batch: 20,
            lease: Duration::from_secs(120),
            give_up_after: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Wait before retry number `attempts` (the attempts made so far).
pub fn backoff(attempts: u32) -> Duration {
    let minutes = match attempts {
        0 | 1 => 1,
        2 => 5,
        3 => 15,
        _ => 60,
    };
    Duration::from_secs(minutes * 60)
}

/// The outbox sender.
pub struct Notifier<D> {
    store: Store,
    deliver: Arc<D>,
    config: NotifierConfig,
    clock: Clock,
    events: Option<EventBus>,
}

impl<D: Deliver> Notifier<D> {
    pub fn new(store: Store, deliver: Arc<D>, config: NotifierConfig) -> Self {
        Self {
            store,
            deliver,
            config,
            clock: system_clock(),
            events: None,
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    /// Wakes the sender as soon as a check produces a transition.
    pub fn with_events(self, events: EventBus) -> Self {
        Self {
            events: Some(events),
            ..self
        }
    }

    /// Delivers every due alert (in batches). Returns how many were attempted.
    pub async fn run_once(&self) -> Result<usize, StoreError> {
        let mut attempted = 0;
        loop {
            let now = (self.clock)();
            let batch = self
                .store
                .claim_outbox(now, self.config.batch, self.config.lease)
                .await?;
            if batch.is_empty() {
                return Ok(attempted);
            }
            attempted += batch.len();
            let sends = batch.iter().map(|delivery| self.attempt(delivery));
            for result in futures_util::future::join_all(sends).await {
                result?;
            }
            if attempted >= 10 * self.config.batch as usize {
                // Leave the rest for the next pass rather than starving shutdown.
                return Ok(attempted);
            }
        }
    }

    #[tracing::instrument(skip_all, fields(delivery = delivery.id, channel = %delivery.channel.spec.name, event = delivery.alert.event.as_str()))]
    async fn attempt(&self, delivery: &Delivery) -> Result<(), StoreError> {
        match self
            .deliver
            .deliver(&delivery.channel.spec, &delivery.alert)
            .await
        {
            Ok(()) => {
                tracing::info!(attempts = delivery.attempts, "alert delivered");
                self.store.mark_delivered(delivery.id, (self.clock)()).await
            }
            Err(error) => {
                let now = (self.clock)();
                let retry_at = self.retry_at(delivery, &error, now);
                match retry_at {
                    Some(at) => {
                        tracing::warn!(error = %error.message, attempts = delivery.attempts, retry_at = %at, "alert delivery failed; will retry")
                    }
                    None => {
                        tracing::error!(error = %error.message, attempts = delivery.attempts, "alert delivery failed; giving up")
                    }
                }
                self.store
                    .mark_delivery_failed(delivery.id, &error.message, retry_at)
                    .await
            }
        }
    }

    fn retry_at(
        &self,
        delivery: &Delivery,
        error: &DeliveryError,
        now: Timestamp,
    ) -> Option<Timestamp> {
        if error.permanent {
            return None;
        }
        let wait = backoff(delivery.attempts).max(error.retry_after.unwrap_or_default());
        let at = now.checked_add(SignedDuration::try_from(wait).ok()?).ok()?;
        let give_up = SignedDuration::try_from(self.config.give_up_after).ok()?;
        let deadline = delivery.created_at.checked_add(give_up).ok()?;
        (at <= deadline).then_some(at)
    }

    /// Delivers due alerts every `poll_interval`, and at once after a
    /// transition, until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut events = self.events.as_ref().map(EventBus::subscribe);
        loop {
            if let Err(error) = self.run_once().await {
                tracing::error!(error = %uptime_domain::Report(&error), "delivering alerts failed");
            }
            let transition = async {
                match &mut events {
                    Some(events) => loop {
                        match events.recv().await {
                            Ok(event) if event.transition.is_some() => break,
                            Ok(_) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed) => std::future::pending::<()>().await,
                        }
                    },
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.config.poll_interval) => {}
                () = transition => {}
            }
        }
    }
}
