//! Shared state for every request.

use std::{fmt, sync::Arc};

use tokio::sync::Notify;
use uptime_domain::Edge;
use uptime_notify::Sender;
use uptime_probe::Prober;
use uptime_runtime::{Cluster, EventBus, VerifyDomain};
use uptime_store::bus::BusMessage;
use uptime_store::{Backend, Store};

use crate::{DomainCache, Hosts, auth::AuthSettings, media::MediaStore, status::PageCache};

/// Everything handlers need. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub hosts: Hosts,
    pub domains: DomainCache,
    pub auth: Arc<AuthSettings>,
    /// Check results as they happen (live dashboard).
    pub events: EventBus,
    /// Runs "Test now" checks. `None` when probing is unavailable.
    pub prober: Option<Arc<Prober>>,
    /// Wakes the scheduler after a monitor changes, so edits apply at once.
    pub scheduler: Option<Arc<Notify>>,
    /// Recently built status pages.
    pub pages: PageCache,
    /// Where custom domains must point (shown as DNS instructions).
    pub edge: Edge,
    /// Checks a custom domain on request. `None` when verification is off.
    pub verifier: Option<Arc<dyn VerifyDomain>>,
    /// Sends "Send test" notifications. `None` when unavailable.
    pub sender: Option<Sender>,
    /// Tells other instances about changes (scheduler wake-ups, domains).
    pub cluster: Option<Cluster>,
    /// Uploaded images. `None` when no storage directory is configured.
    pub media: Option<MediaStore>,
}

impl AppState {
    /// State with auth disabled (no admins) and no prober; configure with the `with_*` methods.
    pub fn new(store: Store, hosts: Hosts) -> Self {
        let edge = Edge {
            host: hosts.edge_host().to_owned(),
            addresses: Vec::new(),
        };
        Self {
            store,
            hosts,
            edge,
            verifier: None,
            sender: None,
            cluster: None,
            media: None,
            domains: DomainCache::default(),
            auth: Arc::new(AuthSettings::disabled()),
            events: EventBus::default(),
            prober: None,
            scheduler: None,
            pages: PageCache::default(),
        }
    }

    #[must_use]
    pub fn with_domains(self, domains: DomainCache) -> Self {
        Self { domains, ..self }
    }

    #[must_use]
    pub fn with_auth(self, auth: AuthSettings) -> Self {
        Self {
            auth: Arc::new(auth),
            ..self
        }
    }

    #[must_use]
    pub fn with_events(self, events: EventBus) -> Self {
        Self { events, ..self }
    }

    #[must_use]
    pub fn with_prober(self, prober: Arc<Prober>) -> Self {
        Self {
            prober: Some(prober),
            ..self
        }
    }

    #[must_use]
    pub fn with_scheduler(self, waker: Arc<Notify>) -> Self {
        Self {
            scheduler: Some(waker),
            ..self
        }
    }

    #[must_use]
    pub fn with_edge(self, edge: Edge) -> Self {
        Self { edge, ..self }
    }

    #[must_use]
    pub fn with_verifier(self, verifier: Arc<dyn VerifyDomain>) -> Self {
        Self {
            verifier: Some(verifier),
            ..self
        }
    }

    #[must_use]
    pub fn with_sender(self, sender: Sender) -> Self {
        Self {
            sender: Some(sender),
            ..self
        }
    }

    #[must_use]
    pub fn with_media(self, media: MediaStore) -> Self {
        Self {
            media: Some(media),
            ..self
        }
    }

    #[must_use]
    pub fn with_cluster(self, cluster: Cluster) -> Self {
        Self {
            cluster: Some(cluster),
            ..self
        }
    }

    /// Asks the scheduler (here and on other instances) to look for due checks now.
    pub(crate) fn wake_scheduler(&self) {
        if let Some(waker) = &self.scheduler {
            waker.notify_one();
        }
        if let Some(cluster) = &self.cluster {
            cluster.publish(BusMessage::Wake);
        }
    }

    /// Tells other instances to reload their verified domains.
    pub(crate) fn domains_changed(&self) {
        if self.store.backend() != Backend::Postgres {
            let domains = self.domains.clone();
            let store = self.store.clone();
            tokio::spawn(async move {
                if let Err(error) = domains.reload(&store).await {
                    tracing::warn!(error = %uptime_domain::Report(&error), "reloading custom domains failed");
                }
            });
        }
        if let Some(cluster) = &self.cluster {
            cluster.publish(BusMessage::DomainsChanged);
        }
    }
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("hosts", &self.hosts)
            .field("auth", &self.auth)
            .field("prober", &self.prober.is_some())
            .field("scheduler", &self.scheduler.is_some())
            .field("edge", &self.edge)
            .field("verifier", &self.verifier.is_some())
            .field("sender", &self.sender.is_some())
            .field("cluster", &self.cluster)
            .finish_non_exhaustive()
    }
}
