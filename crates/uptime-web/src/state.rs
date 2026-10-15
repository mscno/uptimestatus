//! Shared state for every request.

use std::{fmt, sync::Arc};

use tokio::sync::Notify;
use uptime_domain::Edge;
use uptime_notify::Sender;
use uptime_probe::Prober;
use uptime_runtime::{Cluster, EventBus, PageChanges, VerifyDomain};
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
    /// Status pages changed without a check (open pages refresh at once).
    pub page_changes: PageChanges,
    /// Rendered monthly report PDFs.
    pub reports: crate::reports::PdfCache,
    /// Verifies Lettermint's webhook (bounces, complaints). `None` disables it.
    pub lettermint_webhook_secret: Option<String>,
    /// Rate limits for public subscribe forms.
    pub(crate) subscribe_limits: crate::status::subscribe::Limits,
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
            lettermint_webhook_secret: None,
            subscribe_limits: crate::status::subscribe::Limits::default(),
            domains: DomainCache::default(),
            auth: Arc::new(AuthSettings::disabled()),
            events: EventBus::default(),
            prober: None,
            scheduler: None,
            pages: PageCache::default(),
            page_changes: PageChanges::default(),
            reports: crate::reports::PdfCache::default(),
        }
    }

    /// Shares the page-change channel the cluster listener feeds.
    #[must_use]
    pub fn with_page_changes(self, page_changes: PageChanges) -> Self {
        Self {
            page_changes,
            ..self
        }
    }

    /// A status page changed outside a check: rebuild it here, refresh open
    /// copies, and tell the other instances.
    pub(crate) fn page_changed(&self, slug: &str) {
        self.pages.clear();
        self.page_changes.notify(slug);
        if let Some(cluster) = &self.cluster {
            cluster.publish(BusMessage::PageChanged {
                slug: slug.to_owned(),
            });
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
    pub fn with_lettermint_webhook_secret(self, secret: Option<String>) -> Self {
        Self {
            lettermint_webhook_secret: secret.filter(|s| !s.trim().is_empty()),
            ..self
        }
    }

    /// The mailer, when email is configured.
    pub fn mailer(&self) -> Option<&uptime_notify::Mailer> {
        self.sender.as_ref().and_then(Sender::mailer)
    }

    /// Whether email (alerts, subscriptions) is configured.
    pub fn email_enabled(&self) -> bool {
        self.mailer().is_some()
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
