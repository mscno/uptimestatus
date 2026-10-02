//! Notification channels and the alerts sent to them.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{MonitorState, Routing, RoutingError};

/// Where alerts go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// Slack incoming webhook.
    Slack,
    /// Discord channel webhook.
    Discord,
    /// Any HTTP endpoint: JSON body, optional HMAC signature.
    Webhook,
}

impl ChannelKind {
    pub const ALL: [Self; 3] = [Self::Slack, Self::Discord, Self::Webhook];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Webhook => "webhook",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Slack => "Slack",
            Self::Discord => "Discord",
            Self::Webhook => "Webhook",
        }
    }
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChannelKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| format!("unknown channel kind `{s}`"))
    }
}

/// A configured notification channel.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSpec {
    pub name: String,
    pub kind: ChannelKind,
    /// The webhook URL. For Slack and Discord it is itself the secret.
    pub url: Url,
    /// Webhook only: signs each request (`X-Uptimestatus-Signature`).
    #[serde(default)]
    pub secret: Option<String>,
    /// Pre-selected for new monitors.
    #[serde(default)]
    pub default_on: bool,
    /// When and whether this channel hears about alerts.
    #[serde(default, skip_serializing_if = "Routing::is_default")]
    pub routing: Routing,
}

impl fmt::Debug for ChannelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("host", &self.url.host_str())
            .finish_non_exhaustive()
    }
}

/// Why a [`ChannelSpec`] is not usable, per field.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("Give the channel a name.")]
    MissingName,
    #[error("{0} webhook URLs must use https.")]
    InsecureUrl(&'static str),
    #[error("This does not look like a {kind} webhook URL (expected {expected}).")]
    WrongHost {
        kind: &'static str,
        expected: &'static str,
    },
    #[error("Webhook URLs must use http or https.")]
    UnsupportedScheme,
    #[error("Only webhook channels can have a signing secret.")]
    SecretNotSupported,
    #[error(transparent)]
    Routing(#[from] RoutingError),
}

impl ChannelError {
    /// The form field the problem belongs to.
    pub fn field(&self) -> &'static str {
        match self {
            Self::MissingName => "name",
            Self::SecretNotSupported => "secret",
            Self::Routing(error) => error.field(),
            Self::InsecureUrl(_) | Self::WrongHost { .. } | Self::UnsupportedScheme => "url",
        }
    }
}

impl ChannelSpec {
    pub fn validate(&self) -> Result<(), ChannelError> {
        if self.name.trim().is_empty() {
            return Err(ChannelError::MissingName);
        }
        self.routing.validate()?;
        let host = self.url.host_str().unwrap_or_default();
        match self.kind {
            ChannelKind::Slack | ChannelKind::Discord => {
                let (label, expected, hosts): (_, _, &[&str]) = match self.kind {
                    ChannelKind::Slack => {
                        ("Slack", "https://hooks.slack.com/…", &["hooks.slack.com"])
                    }
                    _ => (
                        "Discord",
                        "https://discord.com/api/webhooks/…",
                        &[
                            "discord.com",
                            "discordapp.com",
                            "canary.discord.com",
                            "ptb.discord.com",
                        ],
                    ),
                };
                if self.url.scheme() != "https" {
                    return Err(ChannelError::InsecureUrl(label));
                }
                if !hosts.contains(&host) {
                    return Err(ChannelError::WrongHost {
                        kind: label,
                        expected,
                    });
                }
                if self.secret.is_some() {
                    return Err(ChannelError::SecretNotSupported);
                }
            }
            ChannelKind::Webhook => {
                if !matches!(self.url.scheme(), "http" | "https") {
                    return Err(ChannelError::UnsupportedScheme);
                }
            }
        }
        Ok(())
    }
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertEvent {
    WentDown,
    Recovered,
    /// Still down: periodic reminder.
    Resend,
    /// The TLS certificate expires soon.
    CertExpiring,
    /// "Send test" from the admin console.
    Test,
}

impl AlertEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WentDown => "went_down",
            Self::Recovered => "recovered",
            Self::Resend => "resend",
            Self::CertExpiring => "cert_expiring",
            Self::Test => "test",
        }
    }
}

impl From<crate::Transition> for AlertEvent {
    fn from(transition: crate::Transition) -> Self {
        match transition {
            crate::Transition::WentDown => Self::WentDown,
            crate::Transition::Recovered => Self::Recovered,
            crate::Transition::Resend => Self::Resend,
        }
    }
}

/// The monitor an alert is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertMonitor {
    pub id: i64,
    pub key: String,
    pub name: String,
}

/// One notification, as queued in the outbox and rendered per channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    pub event: AlertEvent,
    pub monitor: AlertMonitor,
    pub state: MonitorState,
    pub previous_state: MonitorState,
    /// Why the check failed (never sent to public pages).
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub status_code: Option<u16>,
    pub at: Timestamp,
    /// Recovered: how long the monitor was down.
    #[serde(default)]
    pub downtime_secs: Option<i64>,
    /// CertExpiring: days left.
    #[serde(default)]
    pub cert_days_left: Option<i64>,
}

impl Alert {
    /// A one-line summary, e.g. "🔴 API is down".
    pub fn headline(&self) -> String {
        let name = &self.monitor.name;
        match self.event {
            AlertEvent::WentDown => format!("🔴 {name} is down"),
            AlertEvent::Resend => format!("🔴 {name} is still down"),
            AlertEvent::Recovered => format!("🟢 {name} is back up"),
            AlertEvent::CertExpiring => match self.cert_days_left {
                Some(days) => format!("🟡 {name}: certificate expires in {days} days"),
                None => format!("🟡 {name}: certificate expires soon"),
            },
            AlertEvent::Test => "🔔 Test notification from uptimestatus".to_owned(),
        }
    }

    /// A test alert for "Send test".
    pub fn test(at: Timestamp) -> Self {
        Self {
            event: AlertEvent::Test,
            monitor: AlertMonitor {
                id: 0,
                key: "test".into(),
                name: "Test monitor".into(),
            },
            state: MonitorState::Up,
            previous_state: MonitorState::Up,
            error: None,
            latency_ms: Some(42),
            status_code: Some(200),
            at,
            downtime_secs: None,
            cert_days_left: None,
        }
    }
}

/// "3h 12m", "45s", "2d 4h": the two largest units of `seconds`.
pub fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let units = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let mut parts = Vec::new();
    let mut rest = seconds;
    for (size, label) in units {
        let value = rest / size;
        rest %= size;
        if value > 0 || (parts.is_empty() && size == 1) {
            parts.push(format!("{value}{label}"));
        }
        if parts.len() == 2 {
            break;
        }
    }
    if parts.len() == 2 && parts[1].starts_with('0') {
        parts.pop();
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    fn spec(kind: ChannelKind, url: &str) -> ChannelSpec {
        ChannelSpec {
            name: "Ops".into(),
            kind,
            url: url.parse().unwrap(),
            secret: None,
            default_on: false,
            routing: Default::default(),
        }
    }

    #[test]
    fn channel_debug_hides_webhook_credentials() {
        let mut channel = spec(
            ChannelKind::Webhook,
            "https://user:password@example.com/private?token=url-secret",
        );
        channel.secret = Some("signing-secret".into());
        let shown = format!("{channel:?}");
        for secret in [
            "user",
            "password",
            "private",
            "url-secret",
            "signing-secret",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[rstest]
    #[case(ChannelKind::Slack, "https://hooks.slack.com/services/T0/B0/xyz")]
    #[case(ChannelKind::Discord, "https://discord.com/api/webhooks/1/abc")]
    #[case(ChannelKind::Discord, "https://discordapp.com/api/webhooks/1/abc")]
    #[case(ChannelKind::Webhook, "https://ops.example.com/hooks/uptime")]
    #[case(ChannelKind::Webhook, "http://ops.example.com/hooks/uptime")]
    fn accepts_channels(#[case] kind: ChannelKind, #[case] url: &str) {
        assert_eq!(spec(kind, url).validate(), Ok(()));
    }

    #[rstest]
    #[case(ChannelKind::Slack, "http://hooks.slack.com/services/x", "url")]
    #[case(ChannelKind::Slack, "https://example.com/services/x", "url")]
    #[case(ChannelKind::Discord, "https://example.com/api/webhooks/1", "url")]
    #[case(ChannelKind::Webhook, "ftp://ops.example.com/x", "url")]
    fn rejects_bad_urls(#[case] kind: ChannelKind, #[case] url: &str, #[case] field: &str) {
        assert_eq!(spec(kind, url).validate().unwrap_err().field(), field);
    }

    #[test]
    fn requires_a_name() {
        let spec = ChannelSpec {
            name: "  ".into(),
            ..spec(ChannelKind::Webhook, "https://x.example.com")
        };
        assert_eq!(spec.validate(), Err(ChannelError::MissingName));
    }

    #[test]
    fn only_webhooks_are_signed() {
        let spec = ChannelSpec {
            secret: Some("s3cret".into()),
            ..spec(ChannelKind::Slack, "https://hooks.slack.com/services/x")
        };
        assert_eq!(spec.validate(), Err(ChannelError::SecretNotSupported));
    }

    #[test]
    fn channel_kinds_round_trip() {
        for kind in ChannelKind::ALL {
            assert_eq!(kind.as_str().parse::<ChannelKind>(), Ok(kind));
        }
        assert!("email".parse::<ChannelKind>().is_err());
    }

    #[test]
    fn alerts_round_trip_through_json() {
        let alert = Alert {
            downtime_secs: Some(90),
            error: Some("connection refused".into()),
            ..Alert::test(Timestamp::from_second(1_800_000_000).unwrap())
        };
        let json = serde_json::to_string(&alert).unwrap();
        assert_eq!(serde_json::from_str::<Alert>(&json).unwrap(), alert);
    }

    #[rstest]
    #[case(AlertEvent::WentDown, "🔴 API is down")]
    #[case(AlertEvent::Resend, "🔴 API is still down")]
    #[case(AlertEvent::Recovered, "🟢 API is back up")]
    fn headlines(#[case] event: AlertEvent, #[case] expected: &str) {
        let alert = Alert {
            event,
            monitor: AlertMonitor {
                id: 1,
                key: "api".into(),
                name: "API".into(),
            },
            ..Alert::test(Timestamp::UNIX_EPOCH)
        };
        assert_eq!(alert.headline(), expected);
    }

    #[rstest]
    #[case(0, "0s")]
    #[case(45, "45s")]
    #[case(60, "1m")]
    #[case(90, "1m 30s")]
    #[case(3_600, "1h")]
    #[case(11_520, "3h 12m")]
    #[case(187_200, "2d 4h")]
    #[case(-5, "0s")]
    fn durations(#[case] seconds: i64, #[case] expected: &str) {
        assert_eq!(format_duration(seconds), expected);
    }
}
