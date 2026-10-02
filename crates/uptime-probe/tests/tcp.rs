#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! TCP connect probes against local listeners.

use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use uptime_domain::{CheckSpec, FailureKind, Observation, TcpCheck};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};

async fn prober(policy: AddressPolicy) -> Prober {
    let prober = Prober::new(ProberConfig {
        policy,
        ..ProberConfig::default()
    })
    .unwrap();
    // Reading the system DNS settings is slow on some machines; do not let it
    // count against a check's timeout.
    prober.warm_up().await;
    prober
}

fn tcp(host: &str, port: u16) -> CheckSpec {
    CheckSpec::Tcp(TcpCheck::connect(host, port))
}

#[tokio::test]
async fn open_port_responds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tcp("127.0.0.1", port), Duration::from_secs(2))
        .await;

    match observation {
        Observation::Responded {
            status_code,
            keyword_found,
            latency,
            ..
        } => {
            assert_eq!((status_code, keyword_found), (None, None));
            assert!(latency < Duration::from_secs(2));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn hostnames_are_resolved() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tcp("localhost", port), Duration::from_secs(2))
        .await;

    assert!(
        matches!(observation, Observation::Responded { .. }),
        "{observation:?}"
    );
}

#[tokio::test]
async fn closed_port_is_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tcp("127.0.0.1", port), Duration::from_secs(2))
        .await;

    assert!(
        matches!(
            observation,
            Observation::Failed {
                kind: FailureKind::Refused,
                ..
            }
        ),
        "{observation:?}"
    );
}

#[tokio::test]
async fn private_targets_are_blocked_by_default() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    for host in ["127.0.0.1", "localhost"] {
        let observation = prober(AddressPolicy::PUBLIC_ONLY)
            .await
            .probe(&tcp(host, port), Duration::from_secs(2))
            .await;
        assert!(
            matches!(
                observation,
                Observation::Failed {
                    kind: FailureKind::Blocked,
                    ..
                }
            ),
            "{host}: {observation:?}"
        );
    }
}

#[tokio::test]
async fn unresolvable_host_is_a_dns_failure() {
    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tcp("does-not-exist.invalid", 443), Duration::from_secs(5))
        .await;
    assert_eq!(
        match observation {
            Observation::Failed { kind, .. } => kind,
            other => panic!("{other:?}"),
        },
        FailureKind::Dns
    );
}

#[tokio::test]
async fn unroutable_address_times_out() {
    // TEST-NET-1 is never routed, so the SYN goes unanswered.
    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tcp("192.0.2.1", 443), Duration::from_millis(300))
        .await;
    assert!(
        matches!(
            observation,
            Observation::Failed {
                kind: FailureKind::Timeout,
                ..
            }
        ),
        "{observation:?}"
    );
}

/// A server that sends `banner` on connect (if any), then answers `PING\r\n` with `+PONG\r\n`.
async fn redis_like(banner: &'static str) -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                if !banner.is_empty() {
                    let _ = stream.write_all(banner.as_bytes()).await;
                }
                let mut buffer = [0u8; 64];
                if let Ok(read) = stream.read(&mut buffer).await
                    && buffer[..read].starts_with(b"PING")
                {
                    let _ = stream.write_all(b"+PONG\r\n").await;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            });
        }
    });
    port
}

async fn talk(port: u16, send: Option<&str>, expect: Option<&str>) -> Observation {
    let check = TcpCheck {
        send: send.map(Into::into),
        expect: expect.map(Into::into),
        ..TcpCheck::connect("127.0.0.1", port)
    };
    prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&CheckSpec::Tcp(check), Duration::from_secs(2))
        .await
}

fn found(observation: &Observation) -> Option<bool> {
    match observation {
        Observation::Responded { keyword_found, .. } => *keyword_found,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn sends_text_and_looks_for_the_reply() {
    let port = redis_like("").await;

    assert_eq!(
        found(&talk(port, Some("PING\\r\\n"), Some("+PONG")).await),
        Some(true)
    );
    assert_eq!(
        found(&talk(port, Some("PING\\r\\n"), Some("+NOPE")).await),
        Some(false)
    );
    assert_eq!(
        found(&talk(port, Some("HELLO\\r\\n"), Some("+PONG")).await),
        Some(false)
    );
}

#[tokio::test]
async fn waits_for_a_banner_without_sending() {
    let port = redis_like("220 mail.example.com ESMTP\r\n").await;

    assert_eq!(found(&talk(port, None, Some("220")).await), Some(true));
    assert_eq!(found(&talk(port, None, Some("SSH-")).await), Some(false));
}

#[tokio::test]
async fn a_silent_server_never_matches_but_does_not_error() {
    let port = redis_like("").await;
    let observation = talk(port, None, Some("hello")).await;

    assert_eq!(found(&observation), Some(false));
}

/// A TLS server (self-signed for `localhost`) that answers `PING` with `+PONG`.
async fn tls_server() -> u16 {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut buffer = [0u8; 64];
                    if tls.read(&mut buffer).await.is_ok() {
                        let _ = tls.write_all(b"+PONG\r\n").await;
                    }
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn tls_checks_report_the_certificate_and_verify_it_unless_told_not_to() {
    let port = tls_server().await;
    let check = |ignore| {
        CheckSpec::Tcp(TcpCheck {
            tls: true,
            ignore_tls_errors: ignore,
            send: Some("PING\\r\\n".into()),
            expect: Some("+PONG".into()),
            ..TcpCheck::connect("localhost", port)
        })
    };
    let prober = prober(AddressPolicy::ALLOW_ALL).await;

    let strict = prober.probe(&check(false), Duration::from_secs(5)).await;
    let lenient = prober.probe(&check(true), Duration::from_secs(5)).await;

    assert!(
        matches!(
            strict,
            Observation::Failed {
                kind: FailureKind::Tls,
                ..
            }
        ),
        "{strict:?}"
    );
    match lenient {
        Observation::Responded {
            keyword_found,
            cert_expires_at,
            ..
        } => {
            assert_eq!(keyword_found, Some(true));
            // rcgen's default validity ends in 4096.
            assert_eq!(cert_expires_at.unwrap().to_string(), "4096-01-01T00:00:00Z");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_plain_port_is_not_a_tls_server() {
    let port = redis_like("").await;
    let check = CheckSpec::Tcp(TcpCheck {
        tls: true,
        ignore_tls_errors: true,
        ..TcpCheck::connect("127.0.0.1", port)
    });

    let observation = prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&check, Duration::from_secs(3))
        .await;

    assert!(
        matches!(
            observation,
            Observation::Failed {
                kind: FailureKind::Tls,
                ..
            }
        ),
        "{observation:?}"
    );
}
