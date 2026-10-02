//! DNS checks: resolve a name and, optionally, look for an expected answer.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use hickory_resolver::{
    TokioResolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
    proto::rr::{Name, RecordType},
};
use uptime_domain::{DnsCheck, DnsRecordType, FailureKind, Observation};

use crate::AddressPolicy;

fn record_type(kind: DnsRecordType) -> RecordType {
    match kind {
        DnsRecordType::A => RecordType::A,
        DnsRecordType::Aaaa => RecordType::AAAA,
        DnsRecordType::Cname => RecordType::CNAME,
        DnsRecordType::Mx => RecordType::MX,
        DnsRecordType::Ns => RecordType::NS,
        DnsRecordType::Txt => RecordType::TXT,
    }
}

fn failed(kind: FailureKind, message: impl Into<String>) -> Observation {
    Observation::Failed {
        kind,
        message: message.into(),
    }
}

/// A fresh, cache-less resolver: `server` (port `port`), or the system's.
async fn resolver(
    server: Option<IpAddr>,
    port: u16,
    timeout: Duration,
) -> Result<TokioResolver, String> {
    let mut builder = match server {
        Some(ip) => {
            let address = SocketAddr::new(ip, port);
            let mut udp = ConnectionConfig::udp();
            udp.port = address.port();
            let mut tcp = ConnectionConfig::tcp();
            tcp.port = address.port();
            TokioResolver::builder_with_config(
                ResolverConfig::from_name_servers(vec![NameServerConfig::new(
                    ip,
                    true,
                    vec![udp, tcp],
                )]),
                TokioRuntimeProvider::default(),
            )
        }
        None => crate::resolve::system_builder().await?,
    };
    let options = builder.options_mut();
    options.cache_size = 0;
    options.timeout = timeout;
    options.attempts = 1;
    builder
        .build()
        .map_err(|e| format!("building the DNS resolver: {e}"))
}

pub(crate) async fn probe(
    policy: AddressPolicy,
    port: u16,
    check: &DnsCheck,
    timeout: Duration,
) -> Observation {
    if let Some(server) = check.resolver
        && !policy.permits(server)
    {
        return failed(
            FailureKind::Blocked,
            format!("resolver {server} is a private or internal address"),
        );
    }
    let resolver = match resolver(check.resolver, port, timeout).await {
        Ok(resolver) => resolver,
        Err(message) => return failed(FailureKind::Io, message),
    };
    let name = match Name::from_ascii(format!("{}.", check.name.trim().trim_end_matches('.'))) {
        Ok(name) => name,
        Err(error) => {
            return failed(
                FailureKind::Dns,
                format!("`{}` is not a DNS name: {error}", check.name),
            );
        }
    };
    let wanted = record_type(check.record_type);
    let started = Instant::now();
    let lookup = match tokio::time::timeout(timeout, resolver.lookup(name, wanted)).await {
        Err(_) => {
            return failed(
                FailureKind::Timeout,
                format!("no DNS answer within {}ms", timeout.as_millis()),
            );
        }
        Ok(Err(error)) if error.is_nx_domain() => {
            return failed(FailureKind::Dns, format!("{} does not exist", check.name));
        }
        Ok(Err(error)) if error.is_no_records_found() => {
            return failed(
                FailureKind::Dns,
                format!("{} has no {} records", check.name, check.record_type),
            );
        }
        Ok(Err(error)) => return failed(FailureKind::Dns, error.to_string()),
        Ok(Ok(lookup)) => lookup,
    };
    let latency = started.elapsed();
    let answers: Vec<String> = lookup
        .answers()
        .iter()
        .filter(|record| record.record_type() == wanted)
        .map(|record| record.data.to_string().trim_end_matches('.').to_owned())
        .collect();
    if answers.is_empty() {
        return failed(
            FailureKind::Dns,
            format!("{} has no {} records", check.name, check.record_type),
        );
    }
    let keyword_found = check.expect.as_ref().map(|expected| {
        let expected = expected.trim().to_lowercase();
        answers
            .iter()
            .any(|answer| answer.to_lowercase().contains(&expected))
    });
    Observation::Responded {
        latency,
        status_code: None,
        keyword_found,
        json_matched: None,
        cert_expires_at: None,
    }
}
