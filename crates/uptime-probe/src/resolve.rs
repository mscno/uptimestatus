//! DNS resolution that enforces the [`AddressPolicy`].
//!
//! Filtering the addresses that are actually connected to (rather than the
//! hostname) also defeats DNS rebinding.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use hickory_resolver::{
    ResolverBuilder, TokioResolver,
    config::{ResolverConfig, ResolverOpts},
    net::runtime::TokioRuntimeProvider,
    system_conf::read_system_conf,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::sync::OnceCell;

use crate::{AddressPolicy, SetupError};

/// The system's DNS settings, read once per process. Reading them is cheap on
/// most systems, but on macOS it asks a system daemon and can take seconds (and
/// far longer when many processes do it at once), so it is done on a blocking
/// thread, at most once, and only when a hostname first needs resolving.
static SYSTEM_CONFIG: OnceCell<(ResolverConfig, ResolverOpts)> = OnceCell::const_new();
static SYSTEM_CONFIG_READS: AtomicUsize = AtomicUsize::new(0);

/// A resolver builder with the system's DNS settings (see [`SYSTEM_CONFIG`]).
/// A failed read is not remembered: the next call tries again.
pub(crate) async fn system_builder() -> Result<ResolverBuilder<TokioRuntimeProvider>, String> {
    let (config, options) = SYSTEM_CONFIG
        .get_or_try_init(|| async {
            SYSTEM_CONFIG_READS.fetch_add(1, Ordering::Relaxed);
            tokio::task::spawn_blocking(read_system_conf)
                .await
                .map_err(|e| format!("reading the system DNS configuration: {e}"))?
                .map_err(|e| format!("reading the system DNS configuration: {e}"))
        })
        .await?;
    let mut builder =
        TokioResolver::builder_with_config(config.clone(), TokioRuntimeProvider::default());
    *builder.options_mut() = options.clone();
    Ok(builder)
}

#[cfg(test)]
fn system_config_reads() -> usize {
    SYSTEM_CONFIG_READS.load(Ordering::Relaxed)
}

/// Why a hostname could not be turned into an address we may connect to.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("{host} resolves only to non-public addresses ({addresses})")]
    Blocked { host: String, addresses: String },
    #[error("cannot resolve {host}: {reason}")]
    Lookup { host: String, reason: String },
}

/// Resolver shared by the HTTP clients and the TCP probe.
#[derive(Clone, Debug)]
pub(crate) struct PolicyResolver {
    /// Built on the first hostname lookup (see [`system_builder`]); IP-literal
    /// targets never need it, and startup does not wait for it.
    resolver: Arc<OnceCell<TokioResolver>>,
    policy: AddressPolicy,
}

impl PolicyResolver {
    pub(crate) fn new(policy: AddressPolicy) -> Result<Self, SetupError> {
        Ok(Self {
            resolver: Arc::new(OnceCell::new()),
            policy,
        })
    }

    async fn resolver(&self) -> Result<&TokioResolver, String> {
        self.resolver
            .get_or_try_init(|| async {
                system_builder()
                    .await?
                    .build()
                    .map_err(|e| format!("building the DNS resolver: {e}"))
            })
            .await
    }

    /// Builds the resolver now instead of at the first hostname lookup.
    pub(crate) async fn warm_up(&self) {
        // A failure is reported by the lookup that needs the resolver.
        let _ = self.resolver().await;
    }

    #[cfg(test)]
    fn is_built(&self) -> bool {
        self.resolver.initialized()
    }

    pub(crate) fn policy(&self) -> AddressPolicy {
        self.policy
    }

    /// Addresses of `host` that the policy permits (IP literals are checked directly).
    pub(crate) async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, ResolveError> {
        let literal = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>();
        let candidates: Vec<IpAddr> = match literal {
            Ok(ip) => vec![ip],
            Err(_) => self
                .resolver()
                .await
                .map_err(|reason| ResolveError::Lookup {
                    host: host.to_owned(),
                    reason,
                })?
                .lookup_ip(host)
                .await
                .map_err(|e| ResolveError::Lookup {
                    host: host.to_owned(),
                    reason: e.to_string(),
                })?
                .iter()
                .collect(),
        };
        if candidates.is_empty() {
            return Err(ResolveError::Lookup {
                host: host.to_owned(),
                reason: "no addresses".to_owned(),
            });
        }
        let permitted: Vec<IpAddr> = candidates
            .iter()
            .copied()
            .filter(|ip| self.policy.permits(*ip))
            .collect();
        if permitted.is_empty() {
            let addresses = candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ResolveError::Blocked {
                host: host.to_owned(),
                addresses,
            });
        }
        Ok(permitted)
    }
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let this = self.clone();
        Box::pin(async move {
            let addresses = PolicyResolver::resolve(&this, name.as_str()).await?;
            let addrs: Addrs = Box::new(addresses.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn building_the_resolver_reads_no_system_configuration() {
        let resolver = PolicyResolver::new(AddressPolicy::ALLOW_ALL).unwrap();
        assert!(!resolver.is_built());
    }

    #[tokio::test]
    async fn ip_literals_never_need_the_resolver() {
        let resolver = PolicyResolver::new(AddressPolicy::ALLOW_ALL).unwrap();
        let found = resolver.resolve("127.0.0.1").await.unwrap();
        assert_eq!(found, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
        let bracketed = resolver.resolve("[::1]").await.unwrap();
        assert_eq!(bracketed, vec!["::1".parse::<IpAddr>().unwrap()]);
        assert!(!resolver.is_built());
    }

    #[tokio::test]
    async fn names_build_the_resolver_on_first_use_and_the_system_config_is_read_once() {
        let first = PolicyResolver::new(AddressPolicy::ALLOW_ALL).unwrap();
        let second = PolicyResolver::new(AddressPolicy::ALLOW_ALL).unwrap();

        // `localhost` comes from the hosts file, so no network is needed.
        assert!(first.resolve("localhost").await.is_ok());
        assert!(second.resolve("localhost").await.is_ok());

        assert!(first.is_built() && second.is_built());
        assert_eq!(system_config_reads(), 1, "read once per process");
    }
}
