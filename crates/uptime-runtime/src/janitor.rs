//! Periodic housekeeping: prunes raw check results and finished alert
//! deliveries past the retention window (90 days by default), and expired admin
//! sessions. Daily counters (the 90-day bars) are kept.

use std::time::Duration;

use jiff::SignedDuration;
use tokio_util::sync::CancellationToken;
use uptime_store::{Store, StoreError};

use crate::{Clock, system_clock};

/// Deletes raw check results and finished alert deliveries older than
/// `retention`, every `every`.
///
/// Daily counters are kept forever. Safe to run on several instances at once.
pub struct Janitor {
    store: Store,
    retention: Duration,
    every: Duration,
    clock: Clock,
}

impl Janitor {
    pub fn new(store: Store, retention: Duration, every: Duration) -> Self {
        Self {
            store,
            retention,
            every,
            clock: system_clock(),
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    /// One pruning pass: old check results and expired sessions.
    /// Returns how many check results were deleted.
    pub async fn run_once(&self) -> Result<u64, StoreError> {
        let retention = SignedDuration::try_from(self.retention).unwrap_or(SignedDuration::MAX);
        let now = (self.clock)();
        let cutoff = now.checked_sub(retention).unwrap_or(now);
        let pruned = self.store.prune_checks_before(cutoff, Self::BATCH).await?;
        self.store.prune_deliveries_before(cutoff).await?;
        let sessions = self.store.prune_sessions(now).await?;
        if sessions > 0 {
            tracing::info!(sessions, "pruned expired sessions");
        }
        Ok(pruned)
    }

    /// Prunes now, then every `every`, until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            if let Err(error) = self.run_once().await {
                tracing::error!(error = %uptime_domain::Report(&error), "pruning old check results failed");
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.every) => {}
            }
        }
    }

    const BATCH: u32 = 10_000;
}
