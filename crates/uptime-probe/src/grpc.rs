//! gRPC health probe: `grpc.health.v1.Health/Check` over HTTP/2.
//!
//! The connection is made here, to an address the policy permits, with TLS
//! (ALPN `h2`) for `https://` servers; tonic speaks HTTP/2 over it and keeps
//! the server's own origin on the request.

use std::{
    error::Error as StdError,
    fmt,
    time::{Duration, Instant},
};

use hyper_util::rt::TokioIo;
use tokio_util::either::Either;
use tonic::{
    Code, Request,
    metadata::{MetadataKey, MetadataValue},
    transport::{Endpoint, Uri},
};
use tonic_health::pb::{
    HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
};
use uptime_domain::{FailureKind, GrpcCheck, Observation};

use crate::{
    resolve::PolicyResolver,
    tcp::{self, Failure},
};

/// A connection failure, carried through tonic's boxed errors.
#[derive(Debug)]
struct ConnectFailure(Failure);

impl fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.1)
    }
}

impl StdError for ConnectFailure {}

pub(crate) async fn probe(
    resolver: &PolicyResolver,
    check: &GrpcCheck,
    timeout: Duration,
) -> Observation {
    let started = Instant::now();
    match tokio::time::timeout(timeout, ask(resolver, check)).await {
        Ok(Ok(status)) if status == ServingStatus::Serving => Observation::Responded {
            latency: started.elapsed(),
            status_code: None,
            response_body: Some(status.as_str_name().to_owned()),
            keyword_found: None,
            json_matched: None,
            cert_expires_at: None,
        },
        Ok(Ok(status)) => Observation::Failed {
            kind: FailureKind::Reported,
            message: format!("health status is {}", status.as_str_name()),
        },
        Ok(Err((kind, message))) => Observation::Failed { kind, message },
        Err(_) => Observation::Failed {
            kind: FailureKind::Timeout,
            message: format!("no answer within {timeout:?}"),
        },
    }
}

async fn ask(resolver: &PolicyResolver, check: &GrpcCheck) -> Result<ServingStatus, Failure> {
    let host = check
        .url
        .host_str()
        .ok_or((FailureKind::Io, "the server URL has no host".to_owned()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = check.port();
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let origin: Uri = format!("{}://{authority}", check.url.scheme())
        .parse()
        .map_err(|e| (FailureKind::Io, format!("invalid server URL: {e}")))?;
    // The connector below does TLS; tonic sees a plain HTTP/2 transport.
    let endpoint = Endpoint::from_shared(format!("http://{authority}"))
        .map_err(|e| (FailureKind::Io, format!("invalid server URL: {e}")))?
        .origin(origin);

    let resolver = resolver.clone();
    let tls = check.tls();
    let ignore_tls_errors = check.ignore_tls_errors;
    let connector = tower::service_fn(move |_: Uri| {
        let resolver = resolver.clone();
        let host = host.clone();
        async move {
            let stream = tcp::connect(&resolver, &host, port)
                .await
                .map_err(ConnectFailure)?;
            let stream = if tls {
                let stream = tcp::tls_connect(stream, &host, ignore_tls_errors, &[b"h2"])
                    .await
                    .map_err(ConnectFailure)?;
                Either::Right(stream)
            } else {
                Either::Left(stream)
            };
            Ok::<_, ConnectFailure>(TokioIo::new(stream))
        }
    });
    let channel = endpoint
        .connect_with_connector(connector)
        .await
        .map_err(|e| transport_failure(&e))?;

    let mut request = Request::new(HealthCheckRequest {
        service: check.service.clone().unwrap_or_default(),
    });
    for (name, value) in &check.metadata {
        let name = MetadataKey::from_bytes(name.as_bytes())
            .map_err(|_| (FailureKind::Io, format!("invalid metadata name `{name}`")))?;
        let value = MetadataValue::try_from(value.as_str())
            .map_err(|_| (FailureKind::Io, "invalid metadata value".to_owned()))?;
        request.metadata_mut().insert(name, value);
    }
    let response = HealthClient::new(channel)
        .check(request)
        .await
        .map_err(|status| status_failure(&status))?;
    Ok(ServingStatus::try_from(response.into_inner().status).unwrap_or(ServingStatus::Unknown))
}

fn status_failure(status: &tonic::Status) -> Failure {
    let message = status.message();
    let kind = match status.code() {
        Code::Unavailable => connect_kind(status).unwrap_or(FailureKind::Refused),
        Code::DeadlineExceeded => FailureKind::Timeout,
        Code::NotFound => {
            return (
                FailureKind::Reported,
                format!("the server does not know the service ({message})"),
            );
        }
        Code::Unimplemented => {
            return (
                FailureKind::Io,
                "the server does not implement grpc.health.v1.Health".to_owned(),
            );
        }
        _ => FailureKind::Io,
    };
    if let Some((kind, text)) = connect_failure(status) {
        return (kind, text);
    }
    (kind, format!("{:?}: {message}", status.code()))
}

fn transport_failure(error: &tonic::transport::Error) -> Failure {
    connect_failure(error).unwrap_or_else(|| (FailureKind::Io, chain(error)))
}

/// Our own connection failure, if one is in `error`'s causes.
fn connect_failure(error: &(dyn StdError + 'static)) -> Option<Failure> {
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(failure) = current.downcast_ref::<ConnectFailure>() {
            return Some(failure.0.clone());
        }
        cause = current.source();
    }
    None
}

fn connect_kind(error: &(dyn StdError + 'static)) -> Option<FailureKind> {
    connect_failure(error).map(|(kind, _)| kind)
}

fn chain(error: &(dyn StdError + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cause = Some(error);
    while let Some(current) = cause {
        let text = current.to_string();
        if !parts.iter().any(|seen| seen.contains(&text)) {
            parts.push(text);
        }
        cause = current.source();
    }
    parts.join(": ")
}
