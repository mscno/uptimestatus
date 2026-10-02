//! TCP probe: connect, and optionally speak TLS, send a line and look for
//! text in the reply.

use std::{
    io,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::TcpStream,
    time::Instant as TokioInstant,
};
use tokio_rustls::TlsConnector;
use uptime_domain::{FailureKind, Observation, TcpCheck};

use crate::resolve::{PolicyResolver, ResolveError};

/// The most reply bytes read while looking for the expected text.
const MAX_REPLY: usize = 4096;

type Failure = (FailureKind, String);

pub(crate) async fn probe(
    resolver: &PolicyResolver,
    check: &TcpCheck,
    timeout: Duration,
) -> Observation {
    let started = Instant::now();
    let deadline = TokioInstant::now() + timeout;
    let attempt = async {
        let addresses = resolver
            .resolve(&check.host)
            .await
            .map_err(|error| match &error {
                ResolveError::Blocked { .. } => (FailureKind::Blocked, error.to_string()),
                ResolveError::Lookup { .. } => (FailureKind::Dns, error.to_string()),
            })?;
        let mut last_error = None;
        let mut stream = None;
        for ip in addresses {
            match TcpStream::connect((ip, check.port)).await {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let Some(stream) = stream else {
            return Err(last_error.map_or_else(
                || (FailureKind::Io, "no address to connect to".to_owned()),
                io_failure,
            ));
        };
        if !check.tls && check.send.is_none() && check.expect.is_none() {
            return Ok(Session {
                latency: started.elapsed(),
                expected_found: None,
                response_body: None,
                cert_expires_at: None,
            });
        }
        converse(stream, check, deadline, started).await
    };

    match tokio::time::timeout(timeout, attempt).await {
        Ok(Ok(session)) => Observation::Responded {
            latency: session.latency,
            status_code: None,
            keyword_found: session.expected_found,
            json_matched: None,
            cert_expires_at: session.cert_expires_at,
            response_body: session.response_body,
        },
        Ok(Err((kind, message))) => Observation::Failed { kind, message },
        Err(_) => Observation::Failed {
            kind: FailureKind::Timeout,
            message: format!("no connection within {timeout:?}"),
        },
    }
}

/// What a talkative check saw.
struct Session {
    latency: Duration,
    expected_found: Option<bool>,
    response_body: Option<String>,
    cert_expires_at: Option<jiff::Timestamp>,
}

/// TLS (if asked), then send, then look for the expected text until `deadline`.
async fn converse(
    stream: TcpStream,
    check: &TcpCheck,
    deadline: TokioInstant,
    started: Instant,
) -> Result<Session, Failure> {
    if !check.tls {
        let (found, response_body) = exchange(stream, check, deadline).await?;
        return Ok(Session {
            latency: started.elapsed(),
            expected_found: found,
            response_body,
            cert_expires_at: None,
        });
    }
    let config = tls_config(check.ignore_tls_errors)?;
    let name = ServerName::try_from(check.host.clone())
        .map_err(|e| (FailureKind::Tls, format!("invalid server name: {e}")))?;
    let tls = TlsConnector::from(config)
        .connect(name, stream)
        .await
        .map_err(|e| (FailureKind::Tls, format!("TLS handshake failed: {e}")))?;
    let cert_expires_at = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(<[_]>::first)
        .and_then(|cert| crate::cert::not_after(cert.as_ref()));
    let (found, response_body) = exchange(tls, check, deadline).await?;
    Ok(Session {
        latency: started.elapsed(),
        expected_found: found,
        response_body,
        cert_expires_at,
    })
}

/// Sends `send`, then reads until `expect` shows up, the peer closes, the
/// reply is long enough, or `deadline`. `None` when nothing is expected.
async fn exchange<S>(
    mut stream: S,
    check: &TcpCheck,
    deadline: TokioInstant,
) -> Result<(Option<bool>, Option<String>), Failure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let send = check.send_bytes();
    if !send.is_empty() {
        stream.write_all(&send).await.map_err(io_failure)?;
        stream.flush().await.map_err(io_failure)?;
    }
    let Some(expect) = check.expect.as_deref().filter(|e| !e.is_empty()) else {
        return Ok((None, None));
    };
    let mut reply = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if String::from_utf8_lossy(&reply).contains(expect) {
            return Ok((
                Some(true),
                Some(String::from_utf8_lossy(&reply).into_owned()),
            ));
        }
        if reply.len() >= MAX_REPLY {
            return Ok((
                Some(false),
                Some(String::from_utf8_lossy(&reply).into_owned()),
            ));
        }
        match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => {
                return Ok((
                    Some(false),
                    Some(String::from_utf8_lossy(&reply).into_owned()),
                ));
            }
            Ok(Ok(read)) => reply.extend_from_slice(&chunk[..read]),
            Ok(Err(error)) => return Err(io_failure(error)),
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Client configs are built once: the platform verifier loads the system's
/// trust store.
fn tls_config(ignore_errors: bool) -> Result<Arc<ClientConfig>, Failure> {
    static STRICT: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    static LENIENT: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let cell = if ignore_errors { &LENIENT } else { &STRICT };
    cell.get_or_init(|| build_config(ignore_errors))
        .clone()
        .map_err(|message| (FailureKind::Io, message))
}

fn build_config(ignore_errors: bool) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if ignore_errors {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider())))
            .with_no_client_auth()
    } else {
        rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(builder)
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// Accepts any certificate (for `ignore_tls_errors`); signatures are still checked.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn io_failure(error: io::Error) -> Failure {
    let kind = match error.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset => FailureKind::Refused,
        io::ErrorKind::TimedOut => FailureKind::Timeout,
        _ => FailureKind::Io,
    };
    (kind, error.to_string())
}
