#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! MQTT probes against a minimal in-process broker.

use std::time::Duration;

use bytes::BytesMut;
use pretty_assertions::assert_eq;
use rumqttc::mqttbytes::{
    QoS,
    v4::{ConnAck, ConnectReturnCode, Packet, Publish, SubAck, SubscribeReasonCode},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use uptime_domain::{CheckSpec, FailureKind, MqttCheck, Observation};
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

/// What the fake broker does with subscriptions.
#[derive(Clone, Copy)]
enum Topic {
    /// Publishes this retained payload at once.
    Retained(&'static str),
    /// Acknowledges, then stays silent.
    Silent,
}

/// Accepts `user`/`secret` (or no login), answers subscriptions per `topic`.
async fn broker(topic: Topic) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(stream, topic));
        }
    });
    port
}

async fn serve(mut stream: TcpStream, topic: Topic) {
    let mut buffer = BytesMut::new();
    loop {
        let packet = loop {
            match Packet::read(&mut buffer, 1 << 20) {
                Ok(packet) => break packet,
                Err(_) => {
                    if stream.read_buf(&mut buffer).await.unwrap_or(0) == 0 {
                        return;
                    }
                }
            }
        };
        let replies = match packet {
            Packet::Connect(connect) => {
                let ok = connect
                    .login
                    .as_ref()
                    .is_none_or(|login| login.username == "user" && login.password == "secret");
                let code = if ok {
                    ConnectReturnCode::Success
                } else {
                    ConnectReturnCode::BadUserNamePassword
                };
                vec![Packet::ConnAck(ConnAck::new(code, false))]
            }
            Packet::Subscribe(subscribe) => {
                let filter = subscribe.filters[0].path.clone();
                let mut replies = vec![Packet::SubAck(SubAck::new(
                    subscribe.pkid,
                    vec![SubscribeReasonCode::Success(QoS::AtMostOnce)],
                ))];
                if let Topic::Retained(payload) = topic {
                    let mut publish = Publish::new(filter, QoS::AtMostOnce, payload);
                    publish.retain = true;
                    replies.push(Packet::Publish(publish));
                }
                replies
            }
            Packet::Disconnect => return,
            _ => Vec::new(),
        };
        for reply in replies {
            let mut bytes = BytesMut::new();
            reply.write(&mut bytes, 1 << 20).unwrap();
            stream.write_all(&bytes).await.unwrap();
        }
    }
}

fn check(port: u16) -> MqttCheck {
    MqttCheck {
        port: Some(port),
        ..MqttCheck::connect("127.0.0.1")
    }
}

#[tokio::test]
async fn a_connack_is_up() {
    let port = broker(Topic::Silent).await;
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&CheckSpec::Mqtt(check(port)), Duration::from_secs(5))
        .await
    {
        Observation::Responded {
            response_body,
            keyword_found,
            ..
        } => {
            assert_eq!((response_body, keyword_found), (None, None));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn subscribed_checks_report_the_first_message() {
    let port = broker(Topic::Retained("status: ok")).await;
    let spec = CheckSpec::Mqtt(MqttCheck {
        username: Some("user".into()),
        password: Some("secret".into()),
        topic: Some("devices/+/health".into()),
        expect: Some("ok".into()),
        ..check(port)
    });
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&spec, Duration::from_secs(5))
        .await
    {
        Observation::Responded {
            response_body,
            keyword_found,
            ..
        } => {
            assert_eq!(response_body.as_deref(), Some("status: ok"));
            assert_eq!(keyword_found, Some(true));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn bad_credentials_are_refused_without_echoing_them() {
    let port = broker(Topic::Silent).await;
    let spec = CheckSpec::Mqtt(MqttCheck {
        username: Some("user".into()),
        password: Some("wrong-password".into()),
        ..check(port)
    });
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&spec, Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, message } => {
            assert_eq!(kind, FailureKind::Refused);
            assert_eq!(
                message,
                "the broker refused the connection: bad username or password"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_silent_topic_times_out() {
    let port = broker(Topic::Silent).await;
    let spec = CheckSpec::Mqtt(MqttCheck {
        topic: Some("health".into()),
        ..check(port)
    });
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&spec, Duration::from_millis(500))
        .await
    {
        Observation::Failed { kind, message } => {
            assert_eq!(kind, FailureKind::Timeout);
            assert!(message.contains("no message on `health`"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn private_brokers_are_blocked_by_default() {
    let port = broker(Topic::Silent).await;
    match prober(AddressPolicy::PUBLIC_ONLY)
        .await
        .probe(&CheckSpec::Mqtt(check(port)), Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Blocked),
        other => panic!("{other:?}"),
    }
}
