//! Custom-domain checks: where a hostname's DNS points, and whether HTTPS
//! works on it (which also makes the edge obtain its certificate early).

use std::{net::SocketAddr, time::Duration};

use hickory_resolver::{
    TokioResolver,
    config::{ConnectionConfig, LookupIpStrategy, NameServerConfig, ResolverConfig},
    net::{NetError, runtime::TokioRuntimeProvider},
    proto::rr::{Name, RData, RecordType},
};
use uptime_domain::{DnsAnswers, Hostname};

use crate::SetupError;

/// Longest CNAME chain followed (loops stop here too).
const MAX_CNAME_HOPS: usize = 8;

/// Looks up custom domains in DNS and over HTTPS.
///
/// Every DNS inspection uses a fresh, cache-less resolver so an admin who has
/// just fixed their records sees the new answers immediately.
#[derive(Clone, Debug)]
pub struct DomainInspector {
    nameserver: Option<SocketAddr>,
    dns_timeout: Duration,
    https: reqwest::Client,
    https_port: u16,
}

impl DomainInspector {
    /// Uses the system resolver and the platform's trusted certificates.
    pub fn new() -> Result<Self, SetupError> {
        let https = reqwest::Client::builder()
            .user_agent(concat!(
                "uptimestatus/",
                env!("CARGO_PKG_VERSION"),
                " (domain check)"
            ))
            // On-demand certificate issuance happens during this request.
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SetupError::new(format!("building the HTTPS client: {e}")))?;
        Ok(Self {
            nameserver: None,
            dns_timeout: Duration::from_secs(5),
            https,
            https_port: 443,
        })
    }

    /// Asks this nameserver instead of the system's.
    pub fn with_nameserver(self, nameserver: SocketAddr) -> Self {
        Self {
            nameserver: Some(nameserver),
            ..self
        }
    }

    pub fn with_dns_timeout(self, dns_timeout: Duration) -> Self {
        Self {
            dns_timeout,
            ..self
        }
    }

    /// Uses `client` for HTTPS checks, connecting to `port` (tests).
    pub fn with_https(self, client: reqwest::Client, port: u16) -> Self {
        Self {
            https: client,
            https_port: port,
            ..self
        }
    }

    /// The CNAME chain and final addresses of `host`. A name that does not
    /// exist has empty answers; `Err` means DNS could not be asked.
    pub async fn dns(&self, host: &Hostname) -> Result<DnsAnswers, String> {
        let resolver = self.resolver().await?;
        let origin = fqdn(host.as_str())?;
        let failed = |error: NetError| format!("DNS lookup for {host} failed: {error}");

        let mut cname_chain = Vec::new();
        let mut name = origin.clone();
        while cname_chain.len() < MAX_CNAME_HOPS {
            let target = match resolver.lookup(name.clone(), RecordType::CNAME).await {
                Ok(lookup) => lookup
                    .answers()
                    .iter()
                    .find_map(|record| match &record.data {
                        RData::CNAME(cname) => Some(cname.0.clone()),
                        _ => None,
                    }),
                Err(error) if error.is_no_records_found() => None,
                Err(error) => return Err(failed(error)),
            };
            let Some(target) = target else { break };
            cname_chain.push(display_name(&target));
            name = target;
        }

        let mut addresses: Vec<_> = match resolver.lookup_ip(origin).await {
            Ok(lookup) => lookup.iter().collect(),
            Err(error) if error.is_no_records_found() => Vec::new(),
            // A CNAME loop has no addresses; the chain already tells the story.
            Err(_) if cname_chain.len() == MAX_CNAME_HOPS => Vec::new(),
            Err(error) => return Err(failed(error)),
        };
        addresses.sort_unstable();
        addresses.dedup();
        Ok(DnsAnswers {
            cname_chain,
            addresses,
        })
    }

    /// Whether `https://{host}/` completes a TLS handshake with a trusted
    /// certificate. Any HTTP status counts as success.
    pub async fn https(&self, host: &Hostname) -> Result<(), String> {
        let url = match self.https_port {
            443 => format!("https://{host}/"),
            port => format!("https://{host}:{port}/"),
        };
        match self.https.get(url).send().await {
            Ok(_) => Ok(()),
            Err(error) => Err(describe_https_error(host, &error)),
        }
    }

    async fn resolver(&self) -> Result<TokioResolver, String> {
        let builder = match self.nameserver {
            Some(address) => {
                let mut connection = ConnectionConfig::udp();
                connection.port = address.port();
                let server = NameServerConfig::new(address.ip(), true, vec![connection]);
                TokioResolver::builder_with_config(
                    ResolverConfig::from_name_servers(vec![server]),
                    TokioRuntimeProvider::default(),
                )
            }
            None => crate::resolve::system_builder().await?,
        };
        let mut builder = builder;
        let options = builder.options_mut();
        options.cache_size = 0;
        options.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
        options.timeout = self.dns_timeout;
        options.attempts = 1;
        builder
            .build()
            .map_err(|e| format!("building the DNS resolver: {e}"))
    }
}

/// `host` as a fully-qualified name, so search domains never apply.
fn fqdn(host: &str) -> Result<Name, String> {
    Name::from_ascii(format!("{host}.")).map_err(|e| format!("`{host}` is not a DNS name: {e}"))
}

fn display_name(name: &Name) -> String {
    name.to_ascii().trim_end_matches('.').to_ascii_lowercase()
}

fn describe_https_error(host: &Hostname, error: &reqwest::Error) -> String {
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    if chain.contains("certificate") || chain.contains("Certificate") {
        format!("https://{host} has no valid certificate yet ({chain})")
    } else if error.is_timeout() {
        format!("https://{host} timed out")
    } else {
        format!("https://{host} is not reachable ({chain})")
    }
}
