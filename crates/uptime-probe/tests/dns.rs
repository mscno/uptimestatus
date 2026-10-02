#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! DNS checks against a local DNS server.

use std::time::Duration;

use pretty_assertions::assert_eq;
use uptime_domain::{CheckSpec, DnsCheck, DnsRecordType, FailureKind, Observation};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};
use uptime_testkit::dns::Zone;

const TIMEOUT: Duration = Duration::from_secs(3);

async fn prober(zone: Zone, policy: AddressPolicy) -> (Prober, std::net::SocketAddr) {
    let server = zone.serve().await;
    let prober = Prober::new(ProberConfig {
        policy,
        dns_port: server.port(),
        ..ProberConfig::default()
    })
    .unwrap();
    (prober, server)
}

fn check(name: &str, record_type: DnsRecordType, expect: Option<&str>) -> CheckSpec {
    CheckSpec::Dns(DnsCheck {
        name: name.into(),
        record_type,
        resolver: Some("127.0.0.1".parse().unwrap()),
        expect: expect.map(str::to_owned),
    })
}

fn zone() -> Zone {
    Zone::default()
        .address("example.test", "203.0.113.7")
        .txt("example.test", "v=spf1 include:_spf.example.net ~all")
        .cname("www.example.test", "example.test")
}

#[tokio::test]
async fn a_resolving_name_is_up() {
    let (prober, _server) = prober(zone(), AddressPolicy::ALLOW_ALL).await;
    let observation = prober
        .probe(&check("example.test", DnsRecordType::A, None), TIMEOUT)
        .await;
    assert!(
        matches!(
            observation,
            Observation::Responded {
                keyword_found: None,
                json_matched: None,
                status_code: None,
                ..
            }
        ),
        "{observation:?}"
    );
}

#[tokio::test]
async fn expected_answers_are_looked_for() {
    let (prober, _server) = prober(zone(), AddressPolicy::ALLOW_ALL).await;
    let found = prober
        .probe(
            &check("example.test", DnsRecordType::Txt, Some("V=SPF1")),
            TIMEOUT,
        )
        .await;
    let missing = prober
        .probe(
            &check("example.test", DnsRecordType::A, Some("198.51.100.1")),
            TIMEOUT,
        )
        .await;
    assert!(
        matches!(
            found,
            Observation::Responded {
                keyword_found: Some(true),
                json_matched: None,
                ..
            }
        ),
        "{found:?}"
    );
    assert!(
        matches!(
            missing,
            Observation::Responded {
                keyword_found: Some(false),
                json_matched: None,
                ..
            }
        ),
        "{missing:?}"
    );
}

#[tokio::test]
async fn missing_names_and_records_are_dns_failures() {
    let (prober, _server) = prober(zone(), AddressPolicy::ALLOW_ALL).await;
    for (name, record_type, expected) in [
        ("nope.example.test", DnsRecordType::A, "does not exist"),
        ("example.test", DnsRecordType::Mx, "has no MX records"),
    ] {
        let observation = prober.probe(&check(name, record_type, None), TIMEOUT).await;
        match observation {
            Observation::Failed { kind, message } => {
                assert_eq!(kind, FailureKind::Dns);
                assert!(message.contains(expected), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn private_resolvers_are_refused_by_the_policy() {
    let (prober, _server) = prober(zone(), AddressPolicy::PUBLIC_ONLY).await;
    let observation = prober
        .probe(&check("example.test", DnsRecordType::A, None), TIMEOUT)
        .await;
    assert!(
        matches!(
            observation,
            Observation::Failed {
                kind: FailureKind::Blocked,
                ..
            }
        ),
        "{observation:?}"
    );
}
