//! Several instances as one: local events go to the others over the Postgres
//! bus ([`uptime_store::bus`]), and theirs are applied here.

use std::{future::Future, pin::Pin, sync::Arc};

use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uptime_domain::{MonitorId, MonitorKey};
use uptime_store::{
    Backend, Store,
    bus::{BusMessage, Listener},
};

use crate::{CheckCompleted, EventBus, events::Forwarder};

/// A callback run when another instance changed the verified domains.
pub type Reload = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// This instance's link to the others.
#[derive(Clone)]
pub struct Cluster {
    store: Store,
    instance: Arc<str>,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

/// What to do with messages from other instances.
#[derive(Clone, Default)]
pub struct Handlers {
    /// Receives other instances' check events (live dashboards).
    pub events: Option<EventBus>,
    /// Wakes this instance's scheduler.
    pub wake: Option<Arc<Notify>>,
    /// Reloads this instance's verified domains.
    pub domains_changed: Option<Reload>,
}

impl Cluster {
    pub fn new(store: Store, instance: &str) -> Self {
        Self {
            store,
            instance: instance.into(),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Tells the other instances, in the background (failures are logged:
    /// the bus only speeds things up; polling catches up regardless).
    pub fn publish(&self, message: BusMessage) {
        if self.store.backend() != Backend::Postgres {
            return;
        }
        let store = self.store.clone();
        let instance = self.instance.clone();
        tokio::spawn(async move {
            if let Err(error) = store.notify(&instance, &message).await {
                tracing::warn!(error = %uptime_domain::Report(&error), "publishing to the bus failed");
            }
        });
    }

    /// An [`EventBus`] forwarder publishing every local check event.
    pub fn forwarder(&self) -> Forwarder {
        let cluster = self.clone();
        Arc::new(move |event: &CheckCompleted| cluster.publish(to_message(event)))
    }

    /// Applies messages from other instances until `shutdown`.
    pub async fn listen(
        self,
        database_url: String,
        handlers: Handlers,
        shutdown: CancellationToken,
    ) {
        let (sink, mut messages) = mpsc::unbounded_channel();
        let listener = tokio::spawn(Listener::new(&database_url, &self.instance).run(
            sink,
            None,
            shutdown.clone(),
        ));
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                message = messages.recv() => {
                    let Some(message) = message else { break };
                    apply(message, &handlers).await;
                }
            }
        }
        let _ = listener.await;
    }
}

/// Applies one message from another instance.
pub async fn apply(message: BusMessage, handlers: &Handlers) {
    match message {
        BusMessage::Check { .. } => {
            if let (Some(events), Some(event)) = (&handlers.events, from_message(message)) {
                events.inject(event);
            }
        }
        BusMessage::Wake => {
            if let Some(wake) = &handlers.wake {
                wake.notify_one();
            }
        }
        BusMessage::DomainsChanged => {
            if let Some(reload) = &handlers.domains_changed {
                reload().await;
            }
        }
    }
}

fn to_message(event: &CheckCompleted) -> BusMessage {
    BusMessage::Check {
        monitor_id: event.monitor_id.0,
        key: event.key.to_string(),
        previous: event.previous,
        state: event.state,
        transition: event.transition,
        latency_ms: event.latency_ms,
        checked_at: event.checked_at,
    }
}

fn from_message(message: BusMessage) -> Option<CheckCompleted> {
    let BusMessage::Check {
        monitor_id,
        key,
        previous,
        state,
        transition,
        latency_ms,
        checked_at,
    } = message
    else {
        return None;
    };
    Some(CheckCompleted {
        monitor_id: MonitorId(monitor_id),
        key: key.parse::<MonitorKey>().ok()?,
        previous,
        state,
        transition,
        latency_ms,
        checked_at,
    })
}
