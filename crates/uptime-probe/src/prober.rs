use std::time::Duration;

use uptime_domain::{CheckSpec, Observation};

use crate::{AddressPolicy, dns, http::HttpProbe, resolve::PolicyResolver, tcp};

/// Settings shared by every probe.
#[derive(Clone, Debug)]
pub struct ProberConfig {
    pub policy: AddressPolicy,
    pub user_agent: String,
    /// Most response body bytes read when looking for a keyword.
    pub max_body_bytes: usize,
    /// Port of resolvers named by DNS checks (53; tests use others).
    pub dns_port: u16,
}

impl Default for ProberConfig {
    fn default() -> Self {
        Self {
            policy: AddressPolicy::PUBLIC_ONLY,
            user_agent: concat!("uptimestatus/", env!("CARGO_PKG_VERSION")).to_owned(),
            max_body_bytes: 1024 * 1024,
            dns_port: 53,
        }
    }
}

/// The probe could not be set up (e.g. no usable DNS configuration).
#[derive(Debug, thiserror::Error)]
#[error("cannot set up the prober: {0}")]
pub struct SetupError(String);

impl SetupError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Runs checks. Cheap to share; holds the resolver and HTTP clients.
#[derive(Debug)]
pub struct Prober {
    resolver: PolicyResolver,
    http: HttpProbe,
    dns_port: u16,
}

impl Prober {
    pub fn new(config: ProberConfig) -> Result<Self, SetupError> {
        let resolver = PolicyResolver::new(config.policy)?;
        let http = HttpProbe::new(resolver.clone(), config.user_agent, config.max_body_bytes);
        Ok(Self {
            resolver,
            http,
            dns_port: config.dns_port,
        })
    }

    /// Prepares name resolution, which reads the system's DNS settings (seconds
    /// on some platforms). Otherwise the first check that needs a hostname pays
    /// for it inside its own timeout. Cheap when already done; failures are
    /// reported by the lookups that need it.
    pub async fn warm_up(&self) {
        self.resolver.warm_up().await;
    }

    /// An HTTP client for outbound requests other than checks (notification
    /// webhooks): names resolve under the same address policy, redirects are
    /// never followed. IP-literal URLs must be checked with [`Self::permits`].
    pub fn outbound_client(
        &self,
        user_agent: &str,
        timeout: Duration,
    ) -> Result<reqwest::Client, SetupError> {
        reqwest::Client::builder()
            .user_agent(user_agent)
            .dns_resolver(self.resolver.clone())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|e| SetupError::new(format!("building the outbound HTTP client: {e}")))
    }

    /// Whether the address policy lets requests reach `ip`.
    pub fn permits(&self, ip: std::net::IpAddr) -> bool {
        self.resolver.policy().permits(ip)
    }

    /// Runs `check`, giving up after `timeout`.
    #[tracing::instrument(skip_all)]
    pub async fn probe(&self, check: &CheckSpec, timeout: Duration) -> Observation {
        match check {
            CheckSpec::Http(http) => self.http.probe(http, timeout).await,
            CheckSpec::Tcp(tcp) => tcp::probe(&self.resolver, tcp, timeout).await,
            CheckSpec::Dns(dns) => {
                dns::probe(self.resolver.policy(), self.dns_port, dns, timeout).await
            }
            // Push monitors are judged from their heartbeats by the scheduler.
            CheckSpec::Push(_) => Observation::Failed {
                kind: uptime_domain::FailureKind::Io,
                message: "push monitors are not probed".into(),
            },
        }
    }
}
