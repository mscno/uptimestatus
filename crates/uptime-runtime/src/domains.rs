//! Custom-domain verification: DNS must point at the edge before a hostname is
//! served (and before the edge may request a certificate for it).

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use jiff::SignedDuration;
use tokio_util::sync::CancellationToken;
use uptime_domain::{DnsAnswers, Edge, Hostname, PageSlug, points_at_edge};
use uptime_store::{CustomDomain, DomainStatus, Store, StoreError};

use crate::{Clock, system_clock};

/// DNS and HTTPS lookups for custom domains. Implemented by
/// [`uptime_probe::DomainInspector`]; tests substitute scripted ones.
pub trait DomainProbe: Send + Sync + 'static {
    /// `Err` means DNS could not be asked (not that the name does not exist).
    fn dns(&self, host: &Hostname) -> impl Future<Output = Result<DnsAnswers, String>> + Send;
    /// Whether HTTPS works on `host` with a trusted certificate.
    fn https(&self, host: &Hostname) -> impl Future<Output = Result<(), String>> + Send;
}

impl DomainProbe for uptime_probe::DomainInspector {
    fn dns(&self, host: &Hostname) -> impl Future<Output = Result<DnsAnswers, String>> + Send {
        uptime_probe::DomainInspector::dns(self, host)
    }

    fn https(&self, host: &Hostname) -> impl Future<Output = Result<(), String>> + Send {
        uptime_probe::DomainInspector::https(self, host)
    }
}

/// Receives the full list of verified domains whenever it may have changed.
pub type DomainSink = Arc<dyn Fn(Vec<(Hostname, PageSlug)>) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct VerifierConfig {
    /// Where custom domains must point.
    pub edge: Edge,
    /// How often unverified domains are retried.
    pub every: Duration,
    /// How long a verification holds before DNS is checked again.
    pub recheck_after: Duration,
    /// Request `https://{domain}/` once DNS is right, so the edge obtains the
    /// certificate before the first visitor does.
    pub prewarm_tls: bool,
}

/// Checks pending domains every [`VerifierConfig::every`] and verified ones
/// daily, publishing the verified set to the [`DomainSink`].
pub struct DomainVerifier<P> {
    store: Store,
    probe: Arc<P>,
    config: VerifierConfig,
    sink: Option<DomainSink>,
    clock: Clock,
}

impl<P> Clone for DomainVerifier<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            probe: self.probe.clone(),
            config: self.config.clone(),
            sink: self.sink.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl<P: DomainProbe> DomainVerifier<P> {
    pub fn new(store: Store, probe: Arc<P>, config: VerifierConfig) -> Self {
        Self {
            store,
            probe,
            config,
            sink: None,
            clock: system_clock(),
        }
    }

    pub fn with_sink(self, sink: DomainSink) -> Self {
        Self {
            sink: Some(sink),
            ..self
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    /// Hands every verified domain to the sink.
    pub async fn publish(&self) -> Result<(), StoreError> {
        if let Some(sink) = &self.sink {
            sink(self.store.verified_domains().await?);
        }
        Ok(())
    }

    /// Checks `domain`'s DNS and records the outcome.
    ///
    /// A failed lookup (resolver down) never takes a verified domain offline;
    /// only an answer pointing elsewhere does.
    pub async fn check(&self, domain: &CustomDomain) -> Result<CustomDomain, StoreError> {
        let result = match self.probe.dns(&domain.hostname).await {
            Ok(answers) => points_at_edge(&domain.hostname, &answers, &self.config.edge),
            Err(error) if domain.status == DomainStatus::Verified => {
                tracing::warn!(domain = %domain.hostname, %error, "DNS lookup failed; keeping the domain verified");
                return Ok(domain.clone());
            }
            Err(error) => Err(error),
        };
        let checked = self
            .store
            .record_domain_check(domain.id, result, (self.clock)())
            .await?;
        let was_served = domain.status == DomainStatus::Verified;
        let is_served = checked.status == DomainStatus::Verified;
        if was_served != is_served {
            tracing::info!(domain = %checked.hostname, status = %checked.status, "custom domain changed");
            self.publish().await?;
        }
        Ok(checked)
    }

    /// Requests the domain over HTTPS and records whether a certificate is served.
    pub async fn prewarm(&self, domain: &CustomDomain) -> Result<CustomDomain, StoreError> {
        let result = self.probe.https(&domain.hostname).await;
        if let Err(error) = &result {
            tracing::warn!(domain = %domain.hostname, %error, "HTTPS pre-warm failed");
        }
        self.store
            .record_certificate(domain.id, result, (self.clock)())
            .await
    }

    /// Checks every due domain (prewarming verified ones). Returns how many were checked.
    pub async fn run_once(&self) -> Result<usize, StoreError> {
        let recheck_after =
            SignedDuration::try_from(self.config.recheck_after).unwrap_or(SignedDuration::MAX);
        let due = self
            .store
            .domains_to_check((self.clock)(), recheck_after)
            .await?;
        for domain in &due {
            let checked = self.check(domain).await?;
            if self.config.prewarm_tls && checked.status == DomainStatus::Verified {
                self.prewarm(&checked).await?;
            }
        }
        Ok(due.len())
    }

    /// Publishes the verified set, then checks due domains every
    /// [`VerifierConfig::every`] until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        if let Err(error) = self.publish().await {
            tracing::error!(error = %uptime_domain::Report(&error), "loading verified custom domains failed");
        }
        loop {
            match self.run_once().await {
                Ok(0) => {}
                Ok(checked) => tracing::debug!(checked, "checked custom domains"),
                Err(error) => {
                    tracing::error!(error = %uptime_domain::Report(&error), "checking custom domains failed")
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.config.every) => {}
            }
        }
    }
}

/// A boxed future, for the object-safe [`VerifyDomain`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Verification on request (the admin console's "Verify" button).
pub trait VerifyDomain: Send + Sync {
    /// Checks DNS now; a newly verified domain is prewarmed in the background.
    fn verify(&self, id: i64) -> BoxFuture<'_, Result<CustomDomain, StoreError>>;
}

impl<P: DomainProbe> VerifyDomain for DomainVerifier<P> {
    fn verify(&self, id: i64) -> BoxFuture<'_, Result<CustomDomain, StoreError>> {
        Box::pin(async move {
            let domain = self
                .store
                .domain(id)
                .await?
                .ok_or(StoreError::DomainNotFound(id))?;
            let checked = self.check(&domain).await?;
            if self.config.prewarm_tls && checked.status == DomainStatus::Verified {
                let verifier = self.clone();
                let target = checked.clone();
                tokio::spawn(async move {
                    if let Err(error) = verifier.prewarm(&target).await {
                        tracing::error!(error = %uptime_domain::Report(&error), "recording the HTTPS pre-warm failed");
                    }
                });
            }
            Ok(checked)
        })
    }
}
