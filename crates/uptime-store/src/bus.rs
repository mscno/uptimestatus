//! A message bus between instances over Postgres `LISTEN`/`NOTIFY`.
//!
//! Every instance listens on one channel. A message carries the sender's
//! instance id, so listeners skip their own. Delivery is best-effort (nothing
//! is lost that matters: check results and domains live in tables; the bus
//! only says "look again" sooner than polling would).

use std::{str::FromStr, sync::Arc, time::Duration};

use futures_util::StreamExt as _;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_postgres::{AsyncMessage, config::SslMode};
use tokio_util::sync::CancellationToken;
use uptime_domain::{MonitorState, Transition};

use crate::{Backend, Result, Store, StoreError};

/// The `LISTEN` channel.
pub const CHANNEL: &str = "uptimestatus";

/// What instances tell each other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BusMessage {
    /// A check completed (for live dashboards on other instances).
    Check {
        monitor_id: i64,
        key: String,
        previous: MonitorState,
        state: MonitorState,
        transition: Option<Transition>,
        latency_ms: Option<i64>,
        checked_at: Timestamp,
    },
    /// Monitors changed or a push arrived: schedulers should look for due checks.
    Wake,
    /// Verified custom domains changed: reload them.
    DomainsChanged,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    from: String,
    #[serde(flatten)]
    message: BusMessage,
}

impl Store {
    /// Sends `message` to every listening instance (including `from`, whose
    /// listener ignores it).
    pub async fn notify(&self, from: &str, message: &BusMessage) -> Result<()> {
        if self.backend() != Backend::Postgres {
            return Ok(());
        }
        let payload = serde_json::to_string(&Envelope {
            from: from.to_owned(),
            message: message.clone(),
        })
        .map_err(|e| StoreError::corrupt("bus message", e))?;
        toasty::sql::query("SELECT pg_notify($1, $2)::text")
            .bind(CHANNEL)
            .bind(payload)
            .exec(&mut self.db())
            .await?;
        Ok(())
    }
}

/// A `LISTEN` connection that reconnects until shut down.
#[derive(Clone)]
pub struct Listener {
    url: Arc<str>,
    instance: Arc<str>,
    retry: Duration,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Listens as `instance` (its own messages are skipped).
    pub fn new(database_url: &str, instance: &str) -> Self {
        Self {
            url: database_url.into(),
            instance: instance.into(),
            retry: Duration::from_secs(2),
        }
    }

    #[must_use]
    pub fn with_retry(self, retry: Duration) -> Self {
        Self { retry, ..self }
    }

    /// Forwards messages from other instances to `sink` until `shutdown`.
    /// `connected` is told each time a `LISTEN` is in place (after reconnects too).
    pub async fn run(
        self,
        sink: mpsc::UnboundedSender<BusMessage>,
        connected: Option<mpsc::UnboundedSender<()>>,
        shutdown: CancellationToken,
    ) {
        loop {
            let session = self.session(&sink, connected.as_ref(), &shutdown);
            match session.await {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(%error, "bus listener disconnected; reconnecting");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.retry) => {}
            }
        }
    }

    /// One connection: `Ok` on shutdown, `Err` when the connection fails.
    async fn session(
        &self,
        sink: &mpsc::UnboundedSender<BusMessage>,
        connected: Option<&mpsc::UnboundedSender<()>>,
        shutdown: &CancellationToken,
    ) -> Result<(), String> {
        let (config, tls) = connection_config(&self.url)?;
        let (client, mut connection) = config.connect(tls).await.map_err(|e| e.to_string())?;
        let (payloads, mut received) = mpsc::unbounded_channel::<String>();
        let driver = tokio::spawn(async move {
            let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
            while let Some(message) = messages.next().await {
                match message {
                    Ok(AsyncMessage::Notification(notification)) => {
                        if payloads.send(notification.payload().to_owned()).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(_) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err("connection closed".to_owned())
        });
        let result = async {
            client
                .batch_execute(&format!("LISTEN {CHANNEL}"))
                .await
                .map_err(|e| e.to_string())?;
            if let Some(connected) = connected {
                let _ = connected.send(());
            }
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return Ok(()),
                    payload = received.recv() => {
                        let Some(payload) = payload else {
                            return Err("connection closed".to_owned());
                        };
                        match serde_json::from_str::<Envelope>(&payload) {
                            Ok(envelope) if *envelope.from == *self.instance => {}
                            Ok(envelope) => {
                                let _ = sink.send(envelope.message);
                            }
                            Err(error) => tracing::warn!(%error, "ignoring an unreadable bus message"),
                        }
                    }
                }
            }
        }
        .await;
        driver.abort();
        drop(client);
        result
    }
}

/// A `tokio-postgres` config and TLS connector for a libpq-style URL, as used
/// with Toasty: `sslmode=disable|prefer|require|verify-ca|verify-full`
/// (the verify modes and `sslrootcert=system` check certificates against the
/// platform's trust store).
fn connection_config(
    url: &str,
) -> Result<
    (
        tokio_postgres::Config,
        tokio_postgres_rustls::MakeRustlsConnect,
    ),
    String,
> {
    let mut parsed = url::Url::parse(url).map_err(|e| format!("invalid database URL: {e}"))?;
    let mut sslmode = "prefer".to_owned();
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .into_owned()
        .filter(|(key, value)| match key.as_str() {
            "sslmode" => {
                sslmode.clone_from(value);
                false
            }
            "sslrootcert" | "sslcert" | "sslkey" | "channel_binding" => false,
            _ => true,
        })
        .collect();
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(kept);
    }
    let mut config =
        tokio_postgres::Config::from_str(parsed.as_str()).map_err(|e| e.to_string())?;
    config.ssl_mode(match sslmode.as_str() {
        "disable" => SslMode::Disable,
        "allow" | "prefer" => SslMode::Prefer,
        _ => SslMode::Require,
    });
    config.connect_timeout(Duration::from_secs(10));
    config.application_name("uptimestatus-bus");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let tls = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(tls)
        .map_err(|e| e.to_string())?
        .with_no_client_auth();
    Ok((config, tokio_postgres_rustls::MakeRustlsConnect::new(tls)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn libpq_tls_options_are_translated() {
        let (config, _) = connection_config(
            "postgresql://u:p@db.example.com:5432/postgres?sslmode=verify-full&sslrootcert=system",
        )
        .unwrap();
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
        assert_eq!(config.get_dbname(), Some("postgres"));
        let (local, _) =
            connection_config("postgres://postgres:postgres@localhost:5433/db").unwrap();
        assert_eq!(local.get_ssl_mode(), SslMode::Prefer);
        let (plain, _) = connection_config("postgres://p@localhost/db?sslmode=disable").unwrap();
        assert_eq!(plain.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn messages_are_tagged_json() {
        let json = serde_json::to_string(&Envelope {
            from: "a".into(),
            message: BusMessage::Wake,
        })
        .unwrap();
        assert_eq!(json, r#"{"from":"a","kind":"wake"}"#);
    }
}
