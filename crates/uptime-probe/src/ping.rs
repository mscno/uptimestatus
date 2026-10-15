//! ICMP echo probe.
//!
//! Uses an unprivileged ICMP datagram socket where the OS allows it (Linux
//! with `net.ipv4.ping_group_range` covering the process's group, macOS), and
//! a raw socket otherwise (root or `CAP_NET_RAW`). One socket per address
//! family is shared by every ping; a random identifier keeps checks apart.

use std::{
    io,
    net::IpAddr,
    time::{Duration, Instant},
};

use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence, SurgeError};
use tokio::sync::OnceCell;
use uptime_domain::{FailureKind, Observation, PingCheck};

use crate::{
    resolve::PolicyResolver,
    tcp::{Failure, resolve_failure},
};

/// Echo payload: the conventional 56 bytes.
const PAYLOAD: [u8; 56] = [0; 56];

/// Lazily opened ICMP sockets. A failure to open one is not remembered, so a
/// later `sysctl` takes effect without a restart.
#[derive(Default)]
pub(crate) struct Pinger {
    v4: OnceCell<Client>,
    v6: OnceCell<Client>,
}

impl std::fmt::Debug for Pinger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pinger")
            .field("v4", &self.v4.initialized())
            .field("v6", &self.v6.initialized())
            .finish()
    }
}

impl Pinger {
    async fn client(&self, ip: IpAddr) -> Result<&Client, Failure> {
        let (cell, kind) = match ip {
            IpAddr::V4(_) => (&self.v4, ICMP::V4),
            IpAddr::V6(_) => (&self.v6, ICMP::V6),
        };
        cell.get_or_try_init(|| async {
            Client::new(&Config::builder().kind(kind).build())
                .map_err(|error| socket_failure(&error, kind))
        })
        .await
    }

    pub(crate) async fn probe(
        &self,
        resolver: &PolicyResolver,
        check: &PingCheck,
        timeout: Duration,
    ) -> Observation {
        match tokio::time::timeout(timeout, self.ping(resolver, check)).await {
            Ok(Ok(observation)) => observation,
            Ok(Err((kind, message))) => Observation::Failed { kind, message },
            Err(_) => Observation::Failed {
                kind: FailureKind::Timeout,
                message: format!("{} packets did not finish within {timeout:?}", check.count),
            },
        }
    }

    async fn ping(
        &self,
        resolver: &PolicyResolver,
        check: &PingCheck,
    ) -> Result<Observation, Failure> {
        let ip = *resolver
            .resolve(check.host.trim())
            .await
            .map_err(resolve_failure)?
            .first()
            .ok_or((FailureKind::Dns, format!("cannot resolve {}", check.host)))?;
        let client = self.client(ip).await?;
        let mut pinger = client.pinger(ip, PingIdentifier(random_u16())).await;
        pinger.timeout(check.packet_timeout);
        let started = Instant::now();
        let mut replies = Vec::new();
        for seq in 0..u16::from(check.count) {
            match pinger.ping(PingSequence(seq), &PAYLOAD).await {
                Ok((_, rtt)) => replies.push(rtt),
                Err(SurgeError::Timeout { .. }) => {}
                Err(SurgeError::IOError(error)) => return Err(io_failure(&error)),
                Err(error) => return Err((FailureKind::Io, error.to_string())),
            }
        }
        let sent = u32::from(check.count);
        let received = u32::try_from(replies.len()).unwrap_or(u32::MAX);
        let lost = sent - received;
        let summary = summary(ip, sent, &replies);
        if check.too_lossy(lost, sent) {
            return Err((FailureKind::Timeout, summary));
        }
        let average = replies.iter().sum::<Duration>() / received.max(1);
        tracing::debug!(%ip, sent, received, elapsed = ?started.elapsed(), "pinged");
        Ok(Observation::Responded {
            latency: average,
            status_code: None,
            response_body: Some(summary),
            keyword_found: None,
            json_matched: None,
            cert_expires_at: None,
        })
    }
}

/// `3 packets to 1.1.1.1: 2 received, 33% loss, rtt min/avg/max 9/10/11 ms`.
fn summary(ip: IpAddr, sent: u32, replies: &[Duration]) -> String {
    let received = u32::try_from(replies.len()).unwrap_or(u32::MAX);
    let loss = (sent - received) * 100 / sent.max(1);
    let mut text = format!("{sent} packets to {ip}: {received} received, {loss}% loss");
    if let (Some(min), Some(max)) = (replies.iter().min(), replies.iter().max()) {
        let avg = replies.iter().sum::<Duration>() / received.max(1);
        text.push_str(&format!(
            ", rtt min/avg/max {}/{}/{} ms",
            min.as_millis(),
            avg.as_millis(),
            max.as_millis()
        ));
    }
    text
}

fn socket_failure(error: &io::Error, kind: ICMP) -> Failure {
    if error.kind() == io::ErrorKind::PermissionDenied {
        let family = if kind == ICMP::V4 { "4" } else { "6" };
        return (
            FailureKind::Io,
            format!(
                "this host does not permit ICMP sockets; allow them with \
                 `sysctl -w net.ipv{family}.ping_group_range=\"0 2147483647\"` or grant CAP_NET_RAW"
            ),
        );
    }
    (
        FailureKind::Io,
        format!("cannot open an ICMP socket: {error}"),
    )
}

fn io_failure(error: &io::Error) -> Failure {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => FailureKind::Blocked,
        io::ErrorKind::TimedOut => FailureKind::Timeout,
        _ => FailureKind::Io,
    };
    (kind, error.to_string())
}

fn random_u16() -> u16 {
    let mut bytes = [0u8; 2];
    // A predictable identifier only risks mixing up two concurrent pings.
    let _ = getrandom::fill(&mut bytes);
    u16::from_ne_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn summaries_report_loss_and_round_trips() {
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        assert_eq!(
            summary(
                ip,
                3,
                &[Duration::from_millis(9), Duration::from_millis(11)]
            ),
            "3 packets to 1.1.1.1: 2 received, 33% loss, rtt min/avg/max 9/10/11 ms"
        );
        assert_eq!(
            summary(ip, 2, &[]),
            "2 packets to 1.1.1.1: 0 received, 100% loss"
        );
    }

    #[test]
    fn a_forbidden_socket_explains_the_fix() {
        let (kind, message) =
            socket_failure(&io::Error::from(io::ErrorKind::PermissionDenied), ICMP::V4);
        assert_eq!(kind, FailureKind::Io);
        assert!(message.contains("net.ipv4.ping_group_range"), "{message}");
    }
}
