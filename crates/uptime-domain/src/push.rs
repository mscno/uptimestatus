//! Push monitors: the service calls in; the scheduler turns the latest call
//! (or its absence) into an [`Observation`] on each check.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

use crate::{FailureKind, Observation};

/// The most recent heartbeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    pub at: Timestamp,
    /// `false` when the service reported itself down (`?status=down`).
    pub up: bool,
    pub message: Option<String>,
    /// The service's own measurement, if it sent one (`?ping=`).
    pub ping: Option<Duration>,
}

/// What the latest push says at `now`, given that one is expected every
/// `interval`. A push counts for one interval plus a grace of a tenth of it
/// (at least 5s), so a service on the same cadence is never judged early.
pub fn observe(last: Option<&Push>, now: Timestamp, interval: Duration) -> Observation {
    let grace = (interval / 10).max(Duration::from_secs(5));
    let window = SignedDuration::try_from(interval + grace).unwrap_or(SignedDuration::MAX);
    match last {
        Some(push) if now.duration_since(push.at) <= window => {
            if push.up {
                Observation::Responded {
                    latency: push.ping.unwrap_or_default(),
                    status_code: None,
                    keyword_found: None,
                    json_matched: None,
                    cert_expires_at: None,
                }
            } else {
                Observation::Failed {
                    kind: FailureKind::Reported,
                    message: push
                        .message
                        .clone()
                        .unwrap_or_else(|| "the service reported a failure".into()),
                }
            }
        }
        Some(push) => Observation::Failed {
            kind: FailureKind::Missed,
            message: format!(
                "no push for {}s (expected every {}s)",
                now.duration_since(push.at).as_secs(),
                interval.as_secs()
            ),
        },
        None => Observation::Failed {
            kind: FailureKind::Missed,
            message: "no push received yet".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn t(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn push(at: i64, up: bool) -> Push {
        Push {
            at: t(at),
            up,
            message: (!up).then(|| "disk full".into()),
            ping: Some(Duration::from_millis(12)),
        }
    }

    fn kind(observation: &Observation) -> Option<FailureKind> {
        match observation {
            Observation::Failed { kind, .. } => Some(*kind),
            Observation::Responded { .. } => None,
        }
    }

    #[test]
    fn a_recent_push_is_up_with_its_ping() {
        let observation = observe(Some(&push(0, true)), t(60), Duration::from_secs(60));
        assert_eq!(
            observation,
            Observation::Responded {
                latency: Duration::from_millis(12),
                status_code: None,
                keyword_found: None,
                json_matched: None,
                cert_expires_at: None,
            }
        );
    }

    #[test]
    fn a_grace_period_covers_a_slightly_late_push() {
        let interval = Duration::from_secs(60);
        assert_eq!(kind(&observe(Some(&push(0, true)), t(66), interval)), None);
        assert_eq!(
            kind(&observe(Some(&push(0, true)), t(67), interval)),
            Some(FailureKind::Missed)
        );
    }

    #[test]
    fn silence_and_reported_failures_are_down() {
        let interval = Duration::from_secs(60);
        assert_eq!(
            kind(&observe(None, t(0), interval)),
            Some(FailureKind::Missed)
        );
        let reported = observe(Some(&push(0, false)), t(10), interval);
        assert_eq!(
            reported,
            Observation::Failed {
                kind: FailureKind::Reported,
                message: "disk full".into()
            }
        );
    }
}
