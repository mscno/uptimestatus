#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! HTTP probes against real local servers.

use std::{sync::Arc, time::Duration};

use pretty_assertions::assert_eq;
use tokio::{io::AsyncWriteExt as _, net::TcpListener};
use uptime_domain::{
    CheckSpec, FailureKind, HttpAuth, HttpCheck, HttpMethod, JsonRule, KeywordRule, Observation,
};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string, header, method, path},
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn prober(policy: AddressPolicy) -> Prober {
    Prober::new(ProberConfig {
        policy,
        ..ProberConfig::default()
    })
    .unwrap()
}

fn get(url: impl AsRef<str>) -> HttpCheck {
    HttpCheck::get(url.as_ref().parse().unwrap())
}

async fn probe(check: HttpCheck) -> Observation {
    prober(AddressPolicy::ALLOW_ALL)
        .probe(&CheckSpec::Http(check), TIMEOUT)
        .await
}

fn failure_kind(observation: &Observation) -> FailureKind {
    match observation {
        Observation::Failed { kind, .. } => *kind,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn status(observation: &Observation) -> u16 {
    match observation {
        Observation::Responded {
            status_code: Some(code),
            ..
        } => *code,
        other => panic!("expected an HTTP response, got {other:?}"),
    }
}

#[tokio::test]
async fn reports_status_and_latency() {
    let server = MockServer::start().await;
    Mock::given(path("/health"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let observation = probe(get(format!("{}/health", server.uri()))).await;

    match observation {
        Observation::Responded {
            latency,
            status_code,
            keyword_found,
            ..
        } => {
            assert_eq!(status_code, Some(204));
            assert_eq!(keyword_found, None, "no keyword rule, body not read");
            assert!(latency > Duration::ZERO && latency < TIMEOUT);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn error_statuses_are_observations_not_failures() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    assert_eq!(status(&probe(get(server.uri())).await), 503);
}

#[tokio::test]
async fn sends_method_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("x-probe", "1"))
        .and(body_string("ping"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/hook"))
        .respond_with(ResponseTemplate::new(418))
        .mount(&server)
        .await;

    let check = HttpCheck {
        method: HttpMethod::Post,
        headers: vec![("x-probe".into(), "1".into())],
        body: Some("ping".into()),
        ..get(format!("{}/hook", server.uri()))
    };

    assert_eq!(status(&probe(check).await), 200);
}

#[tokio::test]
async fn identifies_itself() {
    let server = MockServer::start().await;
    Mock::given(header("user-agent", "uptimestatus-test/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;

    let prober = Prober::new(ProberConfig {
        policy: AddressPolicy::ALLOW_ALL,
        user_agent: "uptimestatus-test/1".into(),
        ..ProberConfig::default()
    })
    .unwrap();

    let observation = prober
        .probe(&CheckSpec::Http(get(server.uri())), TIMEOUT)
        .await;
    assert_eq!(status(&observation), 200);
}

#[tokio::test]
async fn looks_for_the_keyword_in_the_body() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("status: ok"))
        .mount(&server)
        .await;

    for (text, expected) in [("ok", true), ("down", false)] {
        let check = HttpCheck {
            keyword: Some(KeywordRule {
                text: text.into(),
                absent: false,
            }),
            ..get(server.uri())
        };
        match probe(check).await {
            Observation::Responded { keyword_found, .. } => {
                assert_eq!(keyword_found, Some(expected), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn keyword_search_is_capped_to_the_body_limit() {
    let server = MockServer::start().await;
    let body = format!("{}needle", "x".repeat(2048));
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let prober = Prober::new(ProberConfig {
        policy: AddressPolicy::ALLOW_ALL,
        max_body_bytes: 1024,
        ..ProberConfig::default()
    })
    .unwrap();

    let check = HttpCheck {
        keyword: Some(KeywordRule {
            text: "needle".into(),
            absent: false,
        }),
        ..get(server.uri())
    };
    match prober.probe(&CheckSpec::Http(check), TIMEOUT).await {
        Observation::Responded { keyword_found, .. } => assert_eq!(keyword_found, Some(false)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn slow_targets_time_out() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;

    let observation = prober(AddressPolicy::ALLOW_ALL)
        .probe(
            &CheckSpec::Http(get(server.uri())),
            Duration::from_millis(300),
        )
        .await;

    assert_eq!(failure_kind(&observation), FailureKind::Timeout);
}

#[tokio::test]
async fn closed_ports_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    assert_eq!(
        failure_kind(&probe(get(format!("http://{addr}/"))).await),
        FailureKind::Refused
    );
}

#[tokio::test]
async fn follows_redirects_up_to_the_limit() {
    let server = MockServer::start().await;
    Mock::given(path("/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(path("/new"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let follow = get(format!("{}/old", server.uri()));
    let dont_follow = HttpCheck {
        max_redirects: 0,
        ..follow.clone()
    };

    assert_eq!(status(&probe(follow).await), 200);
    assert_eq!(
        status(&probe(dont_follow).await),
        301,
        "max_redirects = 0 reports the redirect itself"
    );
}

#[tokio::test]
async fn redirect_loops_fail() {
    let server = MockServer::start().await;
    Mock::given(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;

    let check = HttpCheck {
        max_redirects: 3,
        ..get(format!("{}/loop", server.uri()))
    };
    let observation = probe(check).await;

    assert_eq!(failure_kind(&observation), FailureKind::Io);
    assert!(
        matches!(&observation, Observation::Failed { message, .. } if message.contains("redirect")),
        "{observation:?}"
    );
}

#[tokio::test]
async fn unresolvable_hosts_are_dns_failures() {
    let observation = probe(get("http://does-not-exist.invalid/")).await;
    assert_eq!(failure_kind(&observation), FailureKind::Dns);
}

#[tokio::test]
async fn public_only_policy_blocks_ip_literals() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let observation = prober(AddressPolicy::PUBLIC_ONLY)
        .probe(&CheckSpec::Http(get(server.uri())), TIMEOUT)
        .await;

    assert_eq!(failure_kind(&observation), FailureKind::Blocked);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "no request may reach the target"
    );
}

#[tokio::test]
async fn public_only_policy_blocks_names_that_resolve_privately() {
    let server = MockServer::start().await;
    let port = server.address().port();

    let check = get(format!("http://localhost:{port}/"));
    let observation = prober(AddressPolicy::PUBLIC_ONLY)
        .probe(&CheckSpec::Http(check), TIMEOUT)
        .await;

    assert_eq!(failure_kind(&observation), FailureKind::Blocked);
}

#[tokio::test]
async fn plain_http_on_an_https_url_is_a_tls_failure() {
    let server = MockServer::start().await;
    let https = server.uri().replace("http://", "https://");
    assert_eq!(failure_kind(&probe(get(https)).await), FailureKind::Tls);
}

/// A TLS server with a self-signed certificate that answers every request with 200.
async fn self_signed_server() -> String {
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
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    format!("https://localhost:{port}/")
}

#[tokio::test]
async fn https_responses_report_when_the_certificate_expires() {
    let url = self_signed_server().await;
    let observation = probe(HttpCheck {
        ignore_tls_errors: true,
        ..get(&url)
    })
    .await;
    match observation {
        // rcgen's default validity ends in 4096.
        Observation::Responded {
            cert_expires_at: Some(at),
            ..
        } => {
            assert_eq!(at.to_string(), "4096-01-01T00:00:00Z");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn plain_http_has_no_certificate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    assert!(matches!(
        probe(get(server.uri())).await,
        Observation::Responded {
            cert_expires_at: None,
            ..
        }
    ));
}

#[tokio::test]
async fn untrusted_certificates_fail_unless_ignored() {
    let url = self_signed_server().await;

    let strict = probe(get(&url)).await;
    let lenient = probe(HttpCheck {
        ignore_tls_errors: true,
        ..get(&url)
    })
    .await;

    assert_eq!(failure_kind(&strict), FailureKind::Tls);
    assert_eq!(status(&lenient), 200);
}

#[tokio::test]
async fn sends_basic_and_bearer_credentials() {
    let server = MockServer::start().await;
    Mock::given(path("/basic"))
        .and(header("authorization", "Basic Ym9iOmh1bnRlcjI="))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/bearer"))
        .and(header("authorization", "Bearer t0ken"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let basic = HttpCheck {
        auth: Some(HttpAuth::Basic {
            username: "bob".into(),
            password: "hunter2".into(),
        }),
        ..get(format!("{}/basic", server.uri()))
    };
    let bearer = HttpCheck {
        auth: Some(HttpAuth::Bearer {
            token: "t0ken".into(),
        }),
        ..get(format!("{}/bearer", server.uri()))
    };

    assert_eq!(status(&probe(basic).await), 200);
    assert_eq!(status(&probe(bearer).await), 200);
}

#[tokio::test]
async fn evaluates_the_json_rule_against_the_body() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"status":"ok"}}"#))
        .mount(&server)
        .await;

    for (expect, matched) in [("ok", true), ("degraded", false)] {
        let check = HttpCheck {
            json: Some(JsonRule {
                path: "data.status".into(),
                expect: Some(expect.into()),
            }),
            ..get(server.uri())
        };
        match probe(check).await {
            Observation::Responded {
                json_matched,
                keyword_found,
                ..
            } => {
                assert_eq!(json_matched, Some(matched), "{expect}");
                assert_eq!(keyword_found, None);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn failed_http_responses_include_a_bounded_body_without_a_content_rule() {
    let server = MockServer::start().await;
    Mock::given(path("/failed"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;
    let observed = probe(get(format!("{}/failed", server.uri()))).await;
    let verdict = uptime_domain::evaluate(
        &CheckSpec::Http(get(format!("{}/failed", server.uri()))),
        &Default::default(),
        &observed,
    );
    assert_eq!(verdict.status_code, Some(503));
    assert_eq!(
        verdict.response_body.as_deref(),
        Some("upstream unavailable")
    );

    Mock::given(path("/large"))
        .respond_with(ResponseTemplate::new(500).set_body_string("é".repeat(10_000)))
        .mount(&server)
        .await;
    let observed = probe(get(format!("{}/large", server.uri()))).await;
    let Observation::Responded {
        response_body: Some(body),
        ..
    } = observed
    else {
        panic!("response")
    };
    assert_eq!(body.len(), 4096);
}

#[tokio::test]
async fn response_excerpt_limit_does_not_shorten_keyword_evaluation() {
    let server = MockServer::start().await;
    Mock::given(path("/large"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{}healthy", "x".repeat(5000))),
        )
        .mount(&server)
        .await;
    let mut check = get(format!("{}/large", server.uri()));
    check.keyword = Some(KeywordRule {
        text: "healthy".into(),
        absent: false,
    });
    let observed = probe(check.clone()).await;
    let verdict = uptime_domain::evaluate(&CheckSpec::Http(check), &Default::default(), &observed);
    assert_eq!(verdict.health, uptime_domain::Health::Up);
    assert_eq!(verdict.response_body, None);
}

#[tokio::test]
async fn incomplete_error_responses_keep_the_status_and_partial_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        tokio::io::AsyncReadExt::read(&mut socket, &mut request)
            .await
            .unwrap();
        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial error").await.unwrap();
        // Ensure the partial body arrives before the connection closes.
        tokio::time::sleep(Duration::from_millis(20)).await;
    });
    let check = get(format!("http://{address}/"));
    let observed = probe(check.clone()).await;
    let verdict = uptime_domain::evaluate(&CheckSpec::Http(check), &Default::default(), &observed);
    assert_eq!(verdict.health, uptime_domain::Health::Down);
    assert_eq!(verdict.status_code, Some(503));
    assert_eq!(verdict.response_body.as_deref(), Some("partial error"));
    assert!(verdict.reason.unwrap().to_string().contains("io:"));
    server.await.unwrap();
}
