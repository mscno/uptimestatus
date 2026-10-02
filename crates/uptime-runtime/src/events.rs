//! In-process event bus. With several instances, [`crate::Cluster`] forwards
//! local events to the others over Postgres and injects theirs here.

use std::sync::Arc;

use jiff::Timestamp;
use tokio::sync::broadcast;
use uptime_domain::{MonitorId, MonitorKey, MonitorState, Transition};

/// Published after every recorded check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckCompleted {
    pub monitor_id: MonitorId,
    pub key: MonitorKey,
    pub previous: MonitorState,
    pub state: MonitorState,
    pub transition: Option<Transition>,
    pub latency_ms: Option<i64>,
    pub checked_at: Timestamp,
}

/// Called with every locally published event (to tell other instances).
pub type Forwarder = Arc<dyn Fn(&CheckCompleted) + Send + Sync>;

/// Fan-out of [`CheckCompleted`] events to any number of subscribers.
///
/// Slow subscribers lose the oldest events rather than blocking the scheduler.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<CheckCompleted>,
    forward: Option<Forwarder>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.sender.receiver_count())
            .field("forwarding", &self.forward.is_some())
            .finish()
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            sender: broadcast::Sender::new(capacity),
            forward: None,
        }
    }

    /// Also hands every [`Self::publish`]ed event to `forward`.
    #[must_use]
    pub fn with_forwarder(self, forward: Forwarder) -> Self {
        Self {
            forward: Some(forward),
            ..self
        }
    }

    /// An event that happened here: delivered locally and forwarded.
    pub fn publish(&self, event: CheckCompleted) {
        if let Some(forward) = &self.forward {
            forward(&event);
        }
        self.inject(event);
    }

    /// An event from another instance: delivered locally only.
    pub fn inject(&self, event: CheckCompleted) {
        // No subscribers is fine: nobody is watching right now.
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CheckCompleted> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
