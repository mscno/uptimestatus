//! MQTT probe (3.1.1): CONNECT and wait for CONNACK; with a topic, SUBSCRIBE
//! and wait for the first message.
//!
//! The packets come from rumqttc's codec; the socket is opened here so the
//! address policy applies to the address actually connected to.

use std::time::{Duration, Instant};

use bytes::BytesMut;
use rumqttc::mqttbytes::{
    self, QoS,
    v4::{ConnectReturnCode, Login, Packet, PubAck, Subscribe, SubscribeReasonCode},
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use uptime_domain::{FailureKind, MqttCheck, Observation};

use crate::{
    excerpt,
    resolve::PolicyResolver,
    tcp::{self, Failure, io_failure},
};

/// Largest packet accepted from the broker.
const MAX_PACKET: usize = 256 * 1024;

/// Seconds the broker may wait for traffic before dropping us.
const KEEP_ALIVE: u16 = 60;

struct Session {
    /// Until CONNACK.
    latency: Duration,
    /// The first message's payload, when subscribed.
    payload: Option<String>,
}

pub(crate) async fn probe(
    resolver: &PolicyResolver,
    check: &MqttCheck,
    timeout: Duration,
) -> Observation {
    let started = Instant::now();
    let attempt = async {
        let host = check.host.trim();
        let stream = tcp::connect(resolver, host, check.port()).await?;
        if check.tls {
            let stream = tcp::tls_connect(stream, host, check.ignore_tls_errors, &[]).await?;
            converse(stream, check, started).await
        } else {
            converse(stream, check, started).await
        }
    };
    match tokio::time::timeout(timeout, attempt).await {
        Ok(Ok(session)) => Observation::Responded {
            latency: session.latency,
            status_code: None,
            keyword_found: check
                .expect
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|expect| {
                    session
                        .payload
                        .as_deref()
                        .unwrap_or_default()
                        .contains(expect)
                }),
            json_matched: None,
            cert_expires_at: None,
            response_body: session.payload.map(excerpt),
        },
        Ok(Err((kind, message))) => Observation::Failed { kind, message },
        Err(_) => Observation::Failed {
            kind: FailureKind::Timeout,
            message: match &check.topic {
                Some(topic) => format!("no message on `{topic}` within {timeout:?}"),
                None => format!("no CONNACK within {timeout:?}"),
            },
        },
    }
}

async fn converse<S>(mut stream: S, check: &MqttCheck, started: Instant) -> Result<Session, Failure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut connect = mqttbytes::v4::Connect::new(client_id(check.client_id.as_deref()));
    connect.keep_alive = KEEP_ALIVE;
    connect.clean_session = true;
    if let Some(username) = &check.username {
        connect.login = Some(Login::new(
            username.clone(),
            check.password.clone().unwrap_or_default(),
        ));
    }
    let mut buffer = BytesMut::new();
    send(&mut stream, &Packet::Connect(connect)).await?;
    match receive(&mut stream, &mut buffer).await? {
        Packet::ConnAck(ack) if ack.code == ConnectReturnCode::Success => {}
        Packet::ConnAck(ack) => {
            return Err((
                FailureKind::Refused,
                format!("the broker refused the connection: {}", refusal(ack.code)),
            ));
        }
        other => return Err(unexpected(&other)),
    }
    let latency = started.elapsed();
    let Some(topic) = check.topic.as_deref().map(str::trim) else {
        let _ = send(&mut stream, &Packet::Disconnect).await;
        return Ok(Session {
            latency,
            payload: None,
        });
    };
    send(
        &mut stream,
        &Packet::Subscribe(Subscribe {
            pkid: 1,
            ..Subscribe::new(topic, QoS::AtLeastOnce)
        }),
    )
    .await?;
    loop {
        match receive(&mut stream, &mut buffer).await? {
            Packet::SubAck(ack) => {
                if ack.return_codes.contains(&SubscribeReasonCode::Failure) {
                    return Err((
                        FailureKind::Reported,
                        format!("the broker refused the subscription to `{topic}`"),
                    ));
                }
            }
            Packet::Publish(publish) => {
                if publish.qos != QoS::AtMostOnce {
                    let _ = send(&mut stream, &Packet::PubAck(PubAck::new(publish.pkid))).await;
                }
                let _ = send(&mut stream, &Packet::Disconnect).await;
                return Ok(Session {
                    latency,
                    payload: Some(String::from_utf8_lossy(&publish.payload).into_owned()),
                });
            }
            Packet::PingResp | Packet::PubRel(_) => {}
            other => return Err(unexpected(&other)),
        }
    }
}

async fn send<S>(stream: &mut S, packet: &Packet) -> Result<(), Failure>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = BytesMut::new();
    packet
        .write(&mut bytes, MAX_PACKET)
        .map_err(|e| (FailureKind::Io, format!("cannot encode {packet:?}: {e}")))?;
    stream.write_all(&bytes).await.map_err(io_failure)?;
    stream.flush().await.map_err(io_failure)
}

/// The next packet, reading more from `stream` as needed.
async fn receive<S>(stream: &mut S, buffer: &mut BytesMut) -> Result<Packet, Failure>
where
    S: AsyncRead + Unpin,
{
    loop {
        match Packet::read(buffer, MAX_PACKET) {
            Ok(packet) => return Ok(packet),
            Err(mqttbytes::Error::InsufficientBytes(_)) => {}
            Err(error) => return Err((FailureKind::Io, format!("invalid MQTT packet: {error}"))),
        }
        let read = stream.read_buf(buffer).await.map_err(io_failure)?;
        if read == 0 {
            return Err((
                FailureKind::Refused,
                "the broker closed the connection".to_owned(),
            ));
        }
    }
}

fn unexpected(packet: &Packet) -> Failure {
    let name = format!("{packet:?}");
    let name = name.split(['(', ' ']).next().unwrap_or("packet");
    (
        FailureKind::Io,
        format!("unexpected {name} from the broker"),
    )
}

fn refusal(code: ConnectReturnCode) -> &'static str {
    match code {
        ConnectReturnCode::Success => "accepted",
        ConnectReturnCode::RefusedProtocolVersion => "unsupported protocol version",
        ConnectReturnCode::BadClientId => "client id rejected",
        ConnectReturnCode::ServiceUnavailable => "service unavailable",
        ConnectReturnCode::BadUserNamePassword => "bad username or password",
        ConnectReturnCode::NotAuthorized => "not authorized",
    }
}

/// `prefix-` plus eight random hex digits, so concurrent checks never kick
/// each other off.
fn client_id(prefix: Option<&str>) -> String {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{suffix}", prefix.unwrap_or("uptimestatus"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_unique_and_prefixed() {
        let first = client_id(Some("gw"));
        assert!(first.starts_with("gw-") && first.len() == 11, "{first}");
        assert_ne!(first, client_id(Some("gw")));
        assert!(client_id(None).starts_with("uptimestatus-"));
    }
}
