#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Custom-domain inspection against a local DNS server and TLS server.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use pretty_assertions::assert_eq;
use tokio::{io::AsyncWriteExt as _, net::TcpListener, net::UdpSocket};
use uptime_domain::{DnsAnswers, Hostname};
use uptime_probe::DomainInspector;
use uptime_testkit::dns::Zone;

fn host(name: &str) -> Hostname {
    name.parse().unwrap()
}

async fn inspector(zone: Zone) -> DomainInspector {
    DomainInspector::new()
        .unwrap()
        .with_nameserver(zone.serve().await)
}

#[tokio::test]
async fn follows_cname_chains_to_the_addresses() {
    let zone = Zone::default()
        .cname("status.team.dev", "alias.team.dev")
        .cname("alias.team.dev", "edge.example.com")
        .address("edge.example.com", "203.0.113.10")
        .address("edge.example.com", "2001:db8::10");

    let answers = inspector(zone).await.dns(&host("status.team.dev")).await;

    assert_eq!(
        answers,
        Ok(DnsAnswers {
            cname_chain: vec!["alias.team.dev".into(), "edge.example.com".into()],
            addresses: vec![
                "203.0.113.10".parse().unwrap(),
                "2001:db8::10".parse().unwrap()
            ],
        })
    );
}

#[tokio::test]
async fn apex_domains_have_only_addresses() {
    let zone = Zone::default().address("team.dev", "203.0.113.10");

    let answers = inspector(zone).await.dns(&host("team.dev")).await;

    assert_eq!(
        answers,
        Ok(DnsAnswers {
            cname_chain: vec![],
            addresses: vec!["203.0.113.10".parse().unwrap()],
        })
    );
}

#[tokio::test]
async fn names_that_do_not_exist_have_no_answers() {
    let answers = inspector(Zone::default())
        .await
        .dns(&host("status.team.dev"))
        .await;
    assert_eq!(answers, Ok(DnsAnswers::default()));
}

#[tokio::test]
async fn cname_loops_stop() {
    let zone = Zone::default()
        .cname("a.team.dev", "b.team.dev")
        .cname("b.team.dev", "a.team.dev");

    let answers = tokio::time::timeout(
        Duration::from_secs(20),
        inspector(zone).await.dns(&host("a.team.dev")),
    )
    .await
    .expect("a CNAME loop must not hang");

    let chain = answers.map(|a| a.cname_chain.len()).unwrap_or_default();
    assert!(chain <= 8, "{chain}");
}

#[tokio::test]
async fn an_unreachable_resolver_is_an_error() {
    // Nothing listens here: the lookup fails rather than claiming "no records".
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let inspector = DomainInspector::new()
        .unwrap()
        .with_nameserver(silent.local_addr().unwrap())
        .with_dns_timeout(Duration::from_millis(300));

    let answers = inspector.dns(&host("status.team.dev")).await;

    assert!(answers.is_err(), "{answers:?}");
}

// ── HTTPS pre-warm ───────────────────────────────────────────────────────

/// A TLS server for `name` whose certificate is signed by the returned CA.
async fn tls_server(name: &str) -> (u16, reqwest::Certificate) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec![name.to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
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
                    let _ = tls
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    (port, reqwest::Certificate::from_der(ca.der()).unwrap())
}

fn client_for(name: &str, port: u16, ca: Option<reqwest::Certificate>) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .resolve(name, SocketAddr::from(([127, 0, 0, 1], port)))
        .timeout(Duration::from_secs(5));
    match ca {
        Some(ca) => builder.tls_certs_only([ca]),
        None => builder,
    }
    .build()
    .unwrap()
}

#[tokio::test]
async fn a_trusted_certificate_passes_whatever_the_status() {
    let (port, ca) = tls_server("status.team.dev").await;
    let inspector = DomainInspector::new()
        .unwrap()
        .with_https(client_for("status.team.dev", port, Some(ca)), port);

    assert_eq!(inspector.https(&host("status.team.dev")).await, Ok(()));
}

#[tokio::test]
async fn an_untrusted_certificate_fails_with_a_reason() {
    let (port, _) = tls_server("status.team.dev").await;
    let inspector = DomainInspector::new()
        .unwrap()
        .with_https(client_for("status.team.dev", port, None), port);

    let error = inspector.https(&host("status.team.dev")).await.unwrap_err();

    assert!(error.contains("certificate"), "{error}");
}
