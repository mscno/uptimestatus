//! Turning an [`Observation`] into a health [`Verdict`] under a [`CheckPolicy`].

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{CheckPolicy, CheckSpec, FailureKind, Observation};

/// Health of a single check after evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Up,
    Degraded,
    Down,
}

impl Health {
    pub const ALL: [Self; 3] = [Self::Up, Self::Degraded, Self::Down];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }
}

/// The stored string was not a known [`Health`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown health `{0}`")]
pub struct UnknownHealth(pub String);

impl std::str::FromStr for Health {
    type Err = UnknownHealth;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|health| health.as_str() == s)
            .ok_or_else(|| UnknownHealth(s.to_owned()))
    }
}

/// Why a check was judged DOWN.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownReason {
    /// The probe itself failed.
    Probe { kind: FailureKind, message: String },
    /// The response status was not in the accepted ranges.
    UnexpectedStatus(u16),
    /// The keyword was required but missing.
    KeywordMissing,
    /// The keyword was forbidden but present.
    KeywordPresent,
    /// The response body did not satisfy the JSON rule.
    JsonMismatch,
    /// Upside-down mode: the target answered although it should not.
    ExpectedFailure,
    /// DNS: no answer contained the expected value.
    UnexpectedAnswer,
}

impl fmt::Display for DownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Probe { kind, message } => write!(f, "{}: {message}", kind.as_str()),
            Self::UnexpectedStatus(code) => write!(f, "unexpected status {code}"),
            Self::KeywordMissing => f.write_str("keyword missing from response"),
            Self::KeywordPresent => f.write_str("forbidden keyword present in response"),
            Self::JsonMismatch => f.write_str("JSON assertion failed"),
            Self::ExpectedFailure => {
                f.write_str("expected the target to be unreachable, but it answered")
            }
            Self::UnexpectedAnswer => f.write_str("no DNS answer contains the expected value"),
        }
    }
}

/// The evaluated result of one check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub health: Health,
    pub latency: Option<Duration>,
    pub status_code: Option<u16>,
    /// Set when `health` is [`Health::Down`].
    pub reason: Option<DownReason>,
}

/// Applies the check's success rules, upside-down mode and the degraded
/// threshold to what the probe observed.
///
/// Order: probe failure → status ranges → keyword → invert → degraded.
/// A [`FailureKind::Blocked`] result is never inverted into success.
pub fn evaluate(check: &CheckSpec, policy: &CheckPolicy, observation: &Observation) -> Verdict {
    let (latency, status_code, raw) = match observation {
        Observation::Failed { kind, message } => (
            None,
            None,
            Err(DownReason::Probe {
                kind: *kind,
                message: message.clone(),
            }),
        ),
        Observation::Responded {
            latency,
            status_code,
            keyword_found,
            json_matched,
            ..
        } => (
            Some(*latency),
            *status_code,
            judge_response(check, *status_code, *keyword_found, *json_matched),
        ),
    };

    let blocked = matches!(
        raw,
        Err(DownReason::Probe {
            kind: FailureKind::Blocked,
            ..
        })
    );
    let outcome = match (policy.invert && !blocked, raw) {
        (false, outcome) => outcome,
        (true, Ok(())) => Err(DownReason::ExpectedFailure),
        (true, Err(_)) => Ok(()),
    };

    match outcome {
        Err(reason) => Verdict {
            health: Health::Down,
            latency,
            status_code,
            reason: Some(reason),
        },
        Ok(()) => {
            let slow = !policy.invert
                && matches!((latency, policy.degraded_after), (Some(l), Some(t)) if l > t);
            let health = if slow { Health::Degraded } else { Health::Up };
            Verdict {
                health,
                latency,
                status_code,
                reason: None,
            }
        }
    }
}

/// Success rules for a target that answered: status ranges, then keyword.
fn judge_response(
    check: &CheckSpec,
    status_code: Option<u16>,
    keyword_found: Option<bool>,
    json_matched: Option<bool>,
) -> Result<(), DownReason> {
    let http = match check {
        CheckSpec::Http(http) => http,
        CheckSpec::Dns(dns) if dns.expect.is_some() && keyword_found != Some(true) => {
            return Err(DownReason::UnexpectedAnswer);
        }
        CheckSpec::Tcp(tcp)
            if tcp.expect.as_deref().is_some_and(|e| !e.is_empty())
                && keyword_found != Some(true) =>
        {
            return Err(DownReason::KeywordMissing);
        }
        _ => return Ok(()),
    };
    if let Some(code) = status_code
        && !http.accepted_status.contains(code)
    {
        return Err(DownReason::UnexpectedStatus(code));
    }
    match (&http.keyword, keyword_found.unwrap_or(false)) {
        (Some(rule), false) if !rule.absent => Err(DownReason::KeywordMissing),
        (Some(rule), true) if rule.absent => Err(DownReason::KeywordPresent),
        _ if http.json.is_some() && json_matched != Some(true) => Err(DownReason::JsonMismatch),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{HttpCheck, KeywordRule, TcpCheck};

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn http() -> HttpCheck {
        HttpCheck::get("https://example.com/health".parse().unwrap())
    }

    fn responded(status: u16, latency_ms: u64) -> Observation {
        Observation::Responded {
            latency: ms(latency_ms),
            status_code: Some(status),
            keyword_found: None,
            json_matched: None,
            cert_expires_at: None,
        }
    }

    fn failed(kind: FailureKind) -> Observation {
        Observation::Failed {
            kind,
            message: "boom".into(),
        }
    }

    fn up(status: Option<u16>, latency_ms: u64) -> Verdict {
        Verdict {
            health: Health::Up,
            latency: Some(ms(latency_ms)),
            status_code: status,
            reason: None,
        }
    }

    fn down(status: Option<u16>, latency: Option<Duration>, reason: DownReason) -> Verdict {
        Verdict {
            health: Health::Down,
            latency,
            status_code: status,
            reason: Some(reason),
        }
    }

    #[test]
    fn accepted_status_is_up() {
        let verdict = evaluate(
            &CheckSpec::Http(http()),
            &CheckPolicy::default(),
            &responded(200, 42),
        );
        assert_eq!(verdict, up(Some(200), 42));
    }

    #[test]
    fn unaccepted_status_is_down() {
        let verdict = evaluate(
            &CheckSpec::Http(http()),
            &CheckPolicy::default(),
            &responded(503, 42),
        );
        assert_eq!(
            verdict,
            down(Some(503), Some(ms(42)), DownReason::UnexpectedStatus(503))
        );
    }

    #[test]
    fn forbidden_can_be_the_expected_status() {
        let check = HttpCheck {
            accepted_status: "403".parse().unwrap(),
            ..http()
        };
        let policy = CheckPolicy::default();
        assert_eq!(
            evaluate(&CheckSpec::Http(check.clone()), &policy, &responded(403, 5)),
            up(Some(403), 5)
        );
        assert_eq!(
            evaluate(&CheckSpec::Http(check), &policy, &responded(200, 5)),
            down(Some(200), Some(ms(5)), DownReason::UnexpectedStatus(200))
        );
    }

    #[test]
    fn probe_failure_is_down_with_the_probe_reason() {
        let verdict = evaluate(
            &CheckSpec::Http(http()),
            &CheckPolicy::default(),
            &failed(FailureKind::Timeout),
        );
        assert_eq!(
            verdict,
            down(
                None,
                None,
                DownReason::Probe {
                    kind: FailureKind::Timeout,
                    message: "boom".into()
                }
            )
        );
    }

    #[test]
    fn required_keyword_must_be_present() {
        let check = HttpCheck {
            keyword: Some(KeywordRule {
                text: "ok".into(),
                absent: false,
            }),
            ..http()
        };
        let policy = CheckPolicy::default();
        let found = Observation::Responded {
            latency: ms(1),
            status_code: Some(200),
            keyword_found: Some(true),
            json_matched: None,
            cert_expires_at: None,
        };
        let missing = Observation::Responded {
            latency: ms(1),
            status_code: Some(200),
            keyword_found: Some(false),
            json_matched: None,
            cert_expires_at: None,
        };
        assert_eq!(
            evaluate(&CheckSpec::Http(check.clone()), &policy, &found).health,
            Health::Up
        );
        assert_eq!(
            evaluate(&CheckSpec::Http(check), &policy, &missing),
            down(Some(200), Some(ms(1)), DownReason::KeywordMissing)
        );
    }

    #[test]
    fn a_failed_json_rule_is_down() {
        let check = HttpCheck {
            json: Some(crate::JsonRule {
                path: "ok".into(),
                expect: None,
            }),
            ..http()
        };
        let policy = CheckPolicy::default();
        let observe = |json_matched| Observation::Responded {
            latency: ms(1),
            status_code: Some(200),
            keyword_found: None,
            json_matched,
            cert_expires_at: None,
        };
        let spec = CheckSpec::Http(check);

        assert_eq!(
            evaluate(&spec, &policy, &observe(Some(true))).health,
            Health::Up
        );
        assert_eq!(
            evaluate(&spec, &policy, &observe(Some(false))),
            down(Some(200), Some(ms(1)), DownReason::JsonMismatch)
        );
    }

    #[test]
    fn forbidden_keyword_must_be_absent() {
        let check = HttpCheck {
            keyword: Some(KeywordRule {
                text: "error".into(),
                absent: true,
            }),
            ..http()
        };
        let policy = CheckPolicy::default();
        let present = Observation::Responded {
            latency: ms(1),
            status_code: Some(200),
            keyword_found: Some(true),
            json_matched: None,
            cert_expires_at: None,
        };
        let absent = Observation::Responded {
            latency: ms(1),
            status_code: Some(200),
            keyword_found: Some(false),
            json_matched: None,
            cert_expires_at: None,
        };
        assert_eq!(
            evaluate(&CheckSpec::Http(check.clone()), &policy, &absent).health,
            Health::Up
        );
        assert_eq!(
            evaluate(&CheckSpec::Http(check), &policy, &present),
            down(Some(200), Some(ms(1)), DownReason::KeywordPresent)
        );
    }

    #[test]
    fn status_is_checked_before_keyword() {
        let check = HttpCheck {
            keyword: Some(KeywordRule {
                text: "ok".into(),
                absent: false,
            }),
            ..http()
        };
        let obs = Observation::Responded {
            latency: ms(1),
            status_code: Some(500),
            keyword_found: Some(false),
            json_matched: None,
            cert_expires_at: None,
        };
        assert_eq!(
            evaluate(&CheckSpec::Http(check), &CheckPolicy::default(), &obs).reason,
            Some(DownReason::UnexpectedStatus(500))
        );
    }

    #[test]
    fn tcp_connect_is_up() {
        let check = CheckSpec::Tcp(TcpCheck::connect("db", 5432));
        let obs = Observation::Responded {
            latency: ms(3),
            status_code: None,
            keyword_found: None,
            json_matched: None,
            cert_expires_at: None,
        };
        assert_eq!(evaluate(&check, &CheckPolicy::default(), &obs), up(None, 3));
    }

    #[test]
    fn tcp_expect_must_be_found() {
        let check = CheckSpec::Tcp(TcpCheck {
            expect: Some("+PONG".into()),
            ..TcpCheck::connect("cache", 6379)
        });
        let observe = |found| Observation::Responded {
            latency: ms(3),
            status_code: None,
            keyword_found: found,
            json_matched: None,
            cert_expires_at: None,
        };
        let policy = CheckPolicy::default();

        assert_eq!(evaluate(&check, &policy, &observe(Some(true))), up(None, 3));
        assert_eq!(
            evaluate(&check, &policy, &observe(Some(false))),
            down(None, Some(ms(3)), DownReason::KeywordMissing)
        );
    }

    #[test]
    fn invert_turns_unreachable_into_up() {
        let policy = CheckPolicy {
            invert: true,
            ..Default::default()
        };
        let verdict = evaluate(
            &CheckSpec::Http(http()),
            &policy,
            &failed(FailureKind::Refused),
        );
        assert_eq!(
            verdict,
            Verdict {
                health: Health::Up,
                latency: None,
                status_code: None,
                reason: None
            }
        );
    }

    #[test]
    fn invert_turns_a_successful_answer_into_down() {
        let policy = CheckPolicy {
            invert: true,
            ..Default::default()
        };
        let verdict = evaluate(&CheckSpec::Http(http()), &policy, &responded(200, 7));
        assert_eq!(
            verdict,
            down(Some(200), Some(ms(7)), DownReason::ExpectedFailure)
        );
    }

    #[test]
    fn invert_turns_a_rejected_status_into_up() {
        let policy = CheckPolicy {
            invert: true,
            ..Default::default()
        };
        let verdict = evaluate(&CheckSpec::Http(http()), &policy, &responded(500, 7));
        assert_eq!(verdict, up(Some(500), 7));
    }

    #[test]
    fn blocked_is_never_inverted() {
        let policy = CheckPolicy {
            invert: true,
            ..Default::default()
        };
        let verdict = evaluate(
            &CheckSpec::Http(http()),
            &policy,
            &failed(FailureKind::Blocked),
        );
        assert_eq!(verdict.health, Health::Down);
    }

    #[test]
    fn slow_success_is_degraded() {
        let policy = CheckPolicy {
            degraded_after: Some(ms(500)),
            ..Default::default()
        };
        let spec = CheckSpec::Http(http());
        assert_eq!(
            evaluate(&spec, &policy, &responded(200, 500)).health,
            Health::Up
        );
        assert_eq!(
            evaluate(&spec, &policy, &responded(200, 501)).health,
            Health::Degraded
        );
    }

    #[test]
    fn inverted_success_is_never_degraded() {
        let policy = CheckPolicy {
            invert: true,
            degraded_after: Some(ms(1)),
            ..Default::default()
        };
        let verdict = evaluate(&CheckSpec::Http(http()), &policy, &responded(500, 900));
        assert_eq!(verdict.health, Health::Up);
    }

    #[test]
    fn health_round_trips_through_strings() {
        for health in Health::ALL {
            assert_eq!(health.as_str().parse::<Health>(), Ok(health));
        }
        assert_eq!("meh".parse::<Health>(), Err(UnknownHealth("meh".into())));
    }

    #[test]
    fn down_reasons_render_readable_messages() {
        assert_eq!(
            DownReason::UnexpectedStatus(502).to_string(),
            "unexpected status 502"
        );
        assert_eq!(
            DownReason::Probe {
                kind: FailureKind::Dns,
                message: "no such host".into()
            }
            .to_string(),
            "dns: no such host"
        );
    }
}
