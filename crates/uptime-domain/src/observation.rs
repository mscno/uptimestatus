//! Raw facts a probe gathered, before any policy is applied.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a probe saw when it ran a check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Observation {
    /// The target answered (a TCP connect or an HTTP response).
    Responded {
        latency: Duration,
        /// HTTP status; `None` for non-HTTP checks.
        status_code: Option<u16>,
        /// Bounded response text for failure diagnostics.
        #[serde(default)]
        response_body: Option<String>,
        /// Whether the keyword was found; `None` when no keyword rule is set.
        keyword_found: Option<bool>,
        /// Whether the JSON rule matched; `None` when no JSON rule is set.
        #[serde(default)]
        json_matched: Option<bool>,
        /// HTTPS: when the server's certificate expires.
        cert_expires_at: Option<jiff::Timestamp>,
    },
    /// HTTP headers arrived, but reading the response failed or timed out.
    FailedResponse {
        latency: Duration,
        status_code: u16,
        response_body: String,
        kind: FailureKind,
        message: String,
    },
    /// The probe could not get an answer.
    Failed { kind: FailureKind, message: String },
}

/// Why a probe failed to get an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Name resolution failed.
    Dns,
    /// The address policy refused the target (private/internal address).
    Blocked,
    /// The connection was refused or reset.
    Refused,
    /// The check exceeded its timeout.
    Timeout,
    /// TLS handshake or certificate validation failed.
    Tls,
    /// Any other I/O or protocol error.
    Io,
    /// Push monitor: no heartbeat arrived in time.
    Missed,
    /// Push monitor: the service reported itself down.
    Reported,
}

impl FailureKind {
    pub const ALL: [Self; 8] = [
        Self::Dns,
        Self::Blocked,
        Self::Refused,
        Self::Timeout,
        Self::Tls,
        Self::Io,
        Self::Missed,
        Self::Reported,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Blocked => "blocked",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Io => "io",
            Self::Missed => "missed",
            Self::Reported => "reported",
        }
    }
}

/// The stored string was not a known [`FailureKind`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown failure kind `{0}`")]
pub struct UnknownFailureKind(pub String);

impl std::str::FromStr for FailureKind {
    type Err = UnknownFailureKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownFailureKind(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kinds_round_trip_through_strings() {
        for kind in FailureKind::ALL {
            assert_eq!(kind.as_str().parse::<FailureKind>(), Ok(kind));
        }
        assert_eq!(
            "gremlins".parse::<FailureKind>(),
            Err(UnknownFailureKind("gremlins".into()))
        );
    }
}
