//! Delivery over HTTP, classifying failures for the retry policy.

use std::{net::IpAddr, sync::Arc, time::Duration};

use reqwest::{StatusCode, header};
use uptime_domain::{Alert, AlertEvent, ChannelKind, ChannelSpec};
use uptime_probe::{Prober, SetupError};
use url::Url;

use crate::render::{discord_payload, signature, slack_payload, webhook_payload};

/// How long one delivery attempt may take.
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RATE_LIMIT_BODY_BYTES: usize = 4096;

/// A failed delivery.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DeliveryError {
    pub message: String,
    /// Retrying cannot help (the webhook is gone, the payload was rejected).
    pub permanent: bool,
    /// The receiver asked us to wait this long (429).
    pub retry_after: Option<Duration>,
}

impl DeliveryError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: false,
            retry_after: None,
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: true,
            retry_after: None,
        }
    }
}

type AddressGuard = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

/// Sends alerts to channels. Cheap to clone.
#[derive(Clone)]
pub struct Sender {
    client: reqwest::Client,
    permits: AddressGuard,
    app_url: Option<Url>,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("app_url", &self.app_url)
            .finish_non_exhaustive()
    }
}

impl Sender {
    /// Sends with `client`, to any address.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            permits: Arc::new(|_| true),
            app_url: None,
        }
    }

    /// Sends under the prober's address policy (private networks stay unreachable).
    pub fn guarded(prober: &Arc<Prober>, user_agent: &str) -> Result<Self, SetupError> {
        let guard = prober.clone();
        Ok(Self {
            client: prober.outbound_client(user_agent, TIMEOUT)?,
            permits: Arc::new(move |ip| guard.permits(ip)),
            app_url: None,
        })
    }

    /// Links alerts to the monitor's page in the console at `app_url`.
    #[must_use]
    pub fn with_app_url(self, app_url: Url) -> Self {
        Self {
            app_url: Some(app_url),
            ..self
        }
    }

    /// Where an alert links to: the monitor's admin page (the console for tests).
    pub fn link(&self, alert: &Alert) -> Option<Url> {
        let base = self.app_url.as_ref()?;
        let path = match alert.event {
            AlertEvent::Test => "/admin".to_owned(),
            _ => format!("/admin/monitors/{}", alert.monitor.id),
        };
        base.join(&path).ok()
    }

    /// Delivers `alert` to `channel`.
    pub async fn send(&self, channel: &ChannelSpec, alert: &Alert) -> Result<(), DeliveryError> {
        if let Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) = channel.url.host() {
            let ip = match channel.url.host() {
                Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4),
                Some(url::Host::Ipv6(v6)) => IpAddr::V6(v6),
                _ => unreachable!("matched above"),
            };
            if !(self.permits)(ip) {
                return Err(DeliveryError::permanent(format!(
                    "{ip} is a private or internal address"
                )));
            }
        }
        let link = self.link(alert);
        let payload = match channel.kind {
            ChannelKind::Slack => slack_payload(alert, link.as_ref()),
            ChannelKind::Discord => discord_payload(alert, link.as_ref()),
            ChannelKind::Webhook => webhook_payload(alert, link.as_ref()),
        };
        let body = serde_json::to_vec(&payload)
            .map_err(|e| DeliveryError::permanent(format!("encoding the payload: {e}")))?;
        let mut request = self
            .client
            .post(channel.url.clone())
            .timeout(TIMEOUT)
            .header(header::CONTENT_TYPE, "application/json");
        if channel.kind == ChannelKind::Webhook {
            let timestamp = jiff::Timestamp::now().as_second();
            request = request
                .header("X-Uptimestatus-Event", alert.event.as_str())
                .header("X-Uptimestatus-Timestamp", timestamp.to_string());
            if let Some(secret) = channel.secret.as_deref().filter(|s| !s.is_empty()) {
                request = request.header(
                    "X-Uptimestatus-Signature",
                    signature(secret, timestamp, &body),
                );
            }
        }

        let response = request.body(body).send().await.map_err(|error| {
            // reqwest errors include the full webhook URL, which may itself be
            // the secret. This message is persisted in delivery history.
            let message = if error.is_timeout() {
                "webhook request timed out"
            } else if error.is_dns() {
                "webhook DNS lookup failed"
            } else if error.is_connect() {
                "webhook connection failed"
            } else {
                "webhook request failed"
            };
            DeliveryError::transient(message)
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let header_wait = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());
        let message = format!("{} answered {status}", channel.kind.label());
        match status {
            StatusCode::TOO_MANY_REQUESTS => {
                // Discord puts the wait (seconds, fractional) in the JSON body.
                let text = limited_body(response, MAX_RATE_LIMIT_BODY_BYTES)
                    .await
                    .unwrap_or_default();
                let body_wait = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|json| json.get("retry_after").and_then(serde_json::Value::as_f64));
                let wait = header_wait
                    .or(body_wait)
                    .filter(|s| s.is_finite() && *s >= 0.0);
                Err(DeliveryError {
                    retry_after: wait.map(Duration::from_secs_f64),
                    ..DeliveryError::transient(message)
                })
            }
            StatusCode::REQUEST_TIMEOUT => Err(DeliveryError::transient(message)),
            status if status.is_client_error() => Err(DeliveryError::permanent(message)),
            _ => Err(DeliveryError::transient(message)),
        }
    }
}

async fn limited_body(mut response: reqwest::Response, limit: usize) -> reqwest::Result<String> {
    let mut body = Vec::new();
    while body.len() < limit {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let take = chunk.len().min(limit - body.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}
