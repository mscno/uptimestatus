//! How often and how strictly a monitor is checked.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Scheduling and evaluation rules for one monitor (Uptime Kuma semantics).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckPolicy {
    /// How often to check while UP or DOWN.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// How often to check while PENDING (unconfirmed failure).
    #[serde(with = "humantime_serde")]
    pub retry_interval: Duration,
    /// Whole-check deadline.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// Consecutive failures tolerated before the monitor is DOWN.
    pub retries: u32,
    /// Flip the result: reachable means DOWN ("upside-down mode").
    pub invert: bool,
    /// Latency above this is reported as DEGRADED instead of UP.
    #[serde(with = "humantime_serde")]
    pub degraded_after: Option<Duration>,
    /// While DOWN, notify again every N failed checks (0 = never).
    pub resend_every: u32,
}

/// Why a [`CheckPolicy`] is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("interval must be between {min:?} and {max:?}")]
    Interval { min: Duration, max: Duration },
    #[error("retry interval must be between {min:?} and {max:?}")]
    RetryInterval { min: Duration, max: Duration },
    #[error("timeout must be at least {min:?} and at most 80% of the interval ({max:?})")]
    Timeout { min: Duration, max: Duration },
    #[error("degraded threshold must be shorter than the timeout")]
    DegradedAfter,
}

impl CheckPolicy {
    pub const MIN_INTERVAL: Duration = Duration::from_secs(20);
    pub const MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
    pub const MIN_RETRY_INTERVAL: Duration = Duration::from_secs(5);
    pub const MIN_TIMEOUT: Duration = Duration::from_secs(1);

    /// Checks the invariants the scheduler and probes rely on.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if !(Self::MIN_INTERVAL..=Self::MAX_INTERVAL).contains(&self.interval) {
            return Err(PolicyError::Interval {
                min: Self::MIN_INTERVAL,
                max: Self::MAX_INTERVAL,
            });
        }
        if !(Self::MIN_RETRY_INTERVAL..=Self::MAX_INTERVAL).contains(&self.retry_interval) {
            return Err(PolicyError::RetryInterval {
                min: Self::MIN_RETRY_INTERVAL,
                max: Self::MAX_INTERVAL,
            });
        }
        let max_timeout = self.interval.mul_f64(0.8);
        if !(Self::MIN_TIMEOUT..=max_timeout).contains(&self.timeout) {
            return Err(PolicyError::Timeout {
                min: Self::MIN_TIMEOUT,
                max: max_timeout,
            });
        }
        if self
            .degraded_after
            .is_some_and(|threshold| threshold >= self.timeout)
        {
            return Err(PolicyError::DegradedAfter);
        }
        Ok(())
    }
}

impl Default for CheckPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(10),
            retries: 1,
            invert: false,
            degraded_after: None,
            resend_every: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn default_policy_is_valid() {
        assert_eq!(CheckPolicy::default().validate(), Ok(()));
    }

    #[test]
    fn interval_bounds_are_enforced() {
        let too_short = CheckPolicy {
            interval: secs(19),
            ..Default::default()
        };
        let too_long = CheckPolicy {
            interval: secs(24 * 60 * 60 + 1),
            ..Default::default()
        };
        let err = PolicyError::Interval {
            min: CheckPolicy::MIN_INTERVAL,
            max: CheckPolicy::MAX_INTERVAL,
        };
        assert_eq!(too_short.validate(), Err(err.clone()));
        assert_eq!(too_long.validate(), Err(err));
    }

    #[test]
    fn retry_interval_bounds_are_enforced() {
        let policy = CheckPolicy {
            retry_interval: secs(4),
            ..Default::default()
        };
        assert_eq!(
            policy.validate(),
            Err(PolicyError::RetryInterval {
                min: CheckPolicy::MIN_RETRY_INTERVAL,
                max: CheckPolicy::MAX_INTERVAL
            })
        );
    }

    #[test]
    fn timeout_must_leave_headroom_in_the_interval() {
        // 80% of 60s = 48s.
        let ok = CheckPolicy {
            timeout: secs(48),
            ..Default::default()
        };
        let too_long = CheckPolicy {
            timeout: secs(49),
            ..Default::default()
        };
        let too_short = CheckPolicy {
            timeout: Duration::from_millis(999),
            ..Default::default()
        };
        let err = PolicyError::Timeout {
            min: CheckPolicy::MIN_TIMEOUT,
            max: secs(48),
        };
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(too_long.validate(), Err(err.clone()));
        assert_eq!(too_short.validate(), Err(err));
    }

    #[test]
    fn degraded_threshold_must_be_below_timeout() {
        let ok = CheckPolicy {
            degraded_after: Some(secs(2)),
            ..Default::default()
        };
        let bad = CheckPolicy {
            degraded_after: Some(secs(10)),
            ..Default::default()
        };
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(bad.validate(), Err(PolicyError::DegradedAfter));
    }

    #[test]
    fn deserializes_human_friendly_durations_with_defaults() {
        let policy: CheckPolicy =
            serde_json::from_str(r#"{"interval":"30s","retries":2,"retry_interval":"10s"}"#)
                .unwrap();
        assert_eq!(
            policy,
            CheckPolicy {
                interval: secs(30),
                retry_interval: secs(10),
                retries: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(serde_json::from_str::<CheckPolicy>(r#"{"intervall":"30s"}"#).is_err());
    }
}
