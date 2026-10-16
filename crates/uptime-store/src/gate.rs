//! A best-effort write gate: while a backup snapshots the database, writes wait
//! and resume as soon as the snapshot finishes.
//!
//! This is a *quiesce*, not the correctness mechanism. A transactionally
//! consistent archive comes from `VACUUM INTO` (see [`crate::Store::snapshot_to`]);
//! the gate only keeps the app from churning the database during the copy, so
//! the wait can be bounded by a short timeout without risking the archive.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

/// Blocks writes while a [`WriteGuard`] is held. Cheap to clone; every clone
/// shares the same gate.
#[derive(Clone, Debug, Default)]
pub struct WriteGate {
    inner: Arc<Gate>,
}

#[derive(Debug, Default)]
struct Gate {
    blocked: AtomicBool,
    notify: Notify,
}

impl WriteGate {
    /// Waits while writes are blocked. Call at the start of a write; a write
    /// that has already passed this point runs to completion.
    pub async fn wait(&self) {
        while self.inner.blocked.load(Ordering::Acquire) {
            // Register before re-checking so a `notify_waiters` cannot be lost.
            let notified = self.inner.notify.notified();
            if !self.inner.blocked.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Blocks writes until the returned guard is dropped.
    pub fn block(&self) -> WriteGuard {
        self.inner.blocked.store(true, Ordering::Release);
        WriteGuard {
            inner: self.inner.clone(),
        }
    }

    /// Whether writes are currently blocked.
    pub fn is_blocked(&self) -> bool {
        self.inner.blocked.load(Ordering::Acquire)
    }
}

/// Reopens the gate when dropped (including on a panic). Created by
/// [`WriteGate::block`].
#[derive(Debug)]
pub struct WriteGuard {
    inner: Arc<Gate>,
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.inner.blocked.store(false, Ordering::Release);
        self.inner.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_blocked_gate_delays_writes_until_released() {
        let gate = WriteGate::default();
        let guard = gate.block();
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                true
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "a blocked gate must delay writes");
        drop(guard);
        let resumed = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the gate must reopen")
            .unwrap();
        assert!(resumed);
        assert!(!gate.is_blocked());
    }

    #[tokio::test]
    async fn an_open_gate_does_not_delay_writes() {
        let gate = WriteGate::default();
        tokio::time::timeout(Duration::from_millis(100), gate.wait())
            .await
            .expect("an open gate must not block");
    }
}
