#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! gRPC health probes against an in-process tonic health service.

use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tonic::transport::{Server, server::TcpIncoming};
use tonic_health::{ServingStatus, server::health_reporter};
use uptime_domain::{CheckSpec, FailureKind, GrpcCheck, Observation};
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

/// A health server: the whole server and `api.v1.Users` serve, `api.v1.Jobs` does not.
async fn server() -> u16 {
    let (reporter, service) = health_reporter();
    reporter
        .set_service_status("api.v1.Users", ServingStatus::Serving)
        .await;
    reporter
        .set_service_status("api.v1.Jobs", ServingStatus::NotServing)
        .await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // The reporter must outlive the server for statuses to stay set.
        let _reporter = reporter;
        Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
            .unwrap();
    });
    port
}

fn check(port: u16, service: Option<&str>) -> CheckSpec {
    CheckSpec::Grpc(GrpcCheck {
        service: service.map(str::to_owned),
        metadata: vec![("authorization".into(), "Bearer test".into())],
        ..GrpcCheck::new(format!("http://127.0.0.1:{port}").parse().unwrap())
    })
}

#[tokio::test]
async fn serving_services_are_up() {
    let port = server().await;
    let prober = prober(AddressPolicy::ALLOW_ALL).await;
    for service in [None, Some("api.v1.Users")] {
        match prober
            .probe(&check(port, service), Duration::from_secs(5))
            .await
        {
            Observation::Responded { response_body, .. } => {
                assert_eq!(response_body.as_deref(), Some("SERVING"));
            }
            other => panic!("{service:?}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn not_serving_and_unknown_services_are_reported_down() {
    let port = server().await;
    let prober = prober(AddressPolicy::ALLOW_ALL).await;
    let jobs = prober
        .probe(&check(port, Some("api.v1.Jobs")), Duration::from_secs(5))
        .await;
    assert_eq!(
        jobs,
        Observation::Failed {
            kind: FailureKind::Reported,
            message: "health status is NOT_SERVING".into(),
        }
    );
    match prober
        .probe(&check(port, Some("api.v1.Nope")), Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, message } => {
            assert_eq!(kind, FailureKind::Reported);
            assert!(message.contains("does not know the service"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn closed_ports_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&check(port, None), Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Refused),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn private_servers_are_blocked_by_default() {
    let port = server().await;
    match prober(AddressPolicy::PUBLIC_ONLY)
        .await
        .probe(&check(port, None), Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Blocked),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn plaintext_servers_fail_a_tls_check() {
    let port = server().await;
    let tls = CheckSpec::Grpc(GrpcCheck::new(
        format!("https://127.0.0.1:{port}").parse().unwrap(),
    ));
    match prober(AddressPolicy::ALLOW_ALL)
        .await
        .probe(&tls, Duration::from_secs(5))
        .await
    {
        Observation::Failed { kind, .. } => assert_eq!(kind, FailureKind::Tls),
        other => panic!("{other:?}"),
    }
}
