#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! ICMP ping probes against loopback.
//!
//! Hosts that forbid unprivileged ICMP sockets (Linux with a closed
//! `net.ipv4.ping_group_range`, no `CAP_NET_RAW`) must explain how to allow
//! them instead of answering.

use std::time::Duration;

use pretty_assertions::assert_eq;
use uptime_domain::{CheckSpec, FailureKind, Observation, PingCheck};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};

async fn prober(policy: AddressPolicy) -> Prober {
    let prober = Prober::new(ProberConfig {
        policy,
        ..ProberConfig::default()
    })
    .unwrap();
    prober.warm_up().await;
    prober
}

fn ping(host: &str) -> CheckSpec {
    CheckSpec::Ping(PingCheck {
        count: 2,
        packet_timeout: Duration::from_millis(500),
        ..PingCheck::host(host)
    })
}

/// Whether this failure is the host forbidding ICMP sockets.
fn forbidden(observation: &Observation) -> bool {
    matches!(observation, Observation::Failed { kind: FailureKind::Io, message }
        if message.contains("ping_group_range"))
}

#[tokio::test]
async fn loopback_answers() {
    let prober = prober(AddressPolicy::ALLOW_ALL).await;
    for host in ["127.0.0.1", "::1"] {
        let observation = prober.probe(&ping(host), Duration::from_secs(5)).await;
        if forbidden(&observation) {
            eprintln!("skipped {host}: this host forbids ICMP sockets ({observation:?})");
            continue;
        }
        match observation {
            Observation::Responded {
                latency,
                response_body,
                ..
            } => {
                assert!(latency < Duration::from_millis(500));
                let summary = response_body.unwrap();
                assert!(
                    summary.starts_with(&format!("2 packets to {host}: 2 received, 0% loss")),
                    "{summary}"
                );
            }
            // Some CI hosts have no IPv6 loopback.
            Observation::Failed { message, .. } if host == "::1" => {
                eprintln!("skipped ::1: {message}");
            }
            other => panic!("{host}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn private_hosts_are_blocked_by_default() {
    let observation = prober(AddressPolicy::PUBLIC_ONLY)
        .await
        .probe(&ping("127.0.0.1"), Duration::from_secs(5))
        .await;
    match observation {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Blocked),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn unresolvable_hosts_are_dns_failures() {
    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&ping("does-not-exist.invalid"), Duration::from_secs(10))
        .await;
    match observation {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Dns),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn unanswered_pings_are_down() {
    // TEST-NET-1 is never routed, so no reply comes back.
    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&ping("192.0.2.1"), Duration::from_secs(5))
        .await;
    if forbidden(&observation) {
        eprintln!("skipped: this host forbids ICMP sockets");
        return;
    }
    match observation {
        Observation::Failed { kind, message } => {
            assert!(
                matches!(kind, FailureKind::Timeout | FailureKind::Io),
                "{kind:?}: {message}"
            );
        }
        other => panic!("{other:?}"),
    }
}
