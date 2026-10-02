//! Per-channel alert routing: escalation delay, quiet hours and muted events.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::AlertEvent;

const DAY_SECS: i64 = 24 * 3600;

/// The longest escalation delay, in minutes.
pub const MAX_ESCALATE_MINS: u32 = 24 * 60;

/// A daily UTC window in which nothing is sent (held until it ends). The
/// window may wrap midnight (`22:00`–`06:00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    /// Minutes after UTC midnight, `0..1440`.
    pub start_min: u16,
    pub end_min: u16,
}

impl QuietHours {
    fn second_of_day(at: Timestamp) -> i64 {
        at.as_second().rem_euclid(DAY_SECS)
    }

    /// Whether `at` falls inside the window (start inclusive, end exclusive).
    pub fn contains(self, at: Timestamp) -> bool {
        let now = Self::second_of_day(at);
        let (start, end) = (i64::from(self.start_min) * 60, i64::from(self.end_min) * 60);
        if start < end {
            start <= now && now < end
        } else {
            now >= start || now < end
        }
    }

    /// `at`, or when the window ends if `at` is inside it.
    pub fn next_open(self, at: Timestamp) -> Timestamp {
        if !self.contains(at) {
            return at;
        }
        let end = i64::from(self.end_min) * 60;
        let now = Self::second_of_day(at);
        let wait = (end - now).rem_euclid(DAY_SECS);
        at.checked_add(SignedDuration::from_secs(wait))
            .unwrap_or(at)
    }
}

/// Why a [`Routing`] is not usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    #[error("Escalation delay can be at most 1440 minutes.")]
    EscalateTooLong,
    #[error("Quiet hours must start and end within a day, at different times.")]
    BadQuietHours,
    #[error("Down alerts cannot be muted.")]
    CannotMuteDown,
}

impl RoutingError {
    /// The form field the problem belongs to.
    pub fn field(&self) -> &'static str {
        match self {
            Self::EscalateTooLong => "escalate_after",
            Self::BadQuietHours => "quiet_start",
            Self::CannotMuteDown => "mute",
        }
    }
}

/// When and whether a channel hears about an alert.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    /// Only alert once a monitor has been down this long (an escalation
    /// tier). Zero alerts immediately.
    #[serde(default)]
    pub escalate_after_mins: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet: Option<QuietHours>,
    /// Events this channel never hears about (besides going down).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mute: Vec<AlertEvent>,
}

impl Routing {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<(), RoutingError> {
        if self.escalate_after_mins > MAX_ESCALATE_MINS {
            return Err(RoutingError::EscalateTooLong);
        }
        if let Some(quiet) = self.quiet
            && (quiet.start_min >= 1440
                || quiet.end_min >= 1440
                || quiet.start_min == quiet.end_min)
        {
            return Err(RoutingError::BadQuietHours);
        }
        if self.mute.contains(&AlertEvent::WentDown) {
            return Err(RoutingError::CannotMuteDown);
        }
        Ok(())
    }

    /// Whether down alerts wait, so recoveries need care (see the store).
    pub fn escalates(&self) -> bool {
        self.escalate_after_mins > 0
    }

    /// When to send an alert that happened at `at`; `None` drops it. Tests
    /// always go out at once.
    pub fn send_at(&self, event: AlertEvent, at: Timestamp) -> Option<Timestamp> {
        if event == AlertEvent::Test {
            return Some(at);
        }
        if self.mute.contains(&event) {
            return None;
        }
        let due = if event == AlertEvent::WentDown {
            at.checked_add(SignedDuration::from_mins(i64::from(
                self.escalate_after_mins,
            )))
            .unwrap_or(at)
        } else {
            at
        };
        Some(self.quiet.map_or(due, |quiet| quiet.next_open(due)))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn at(hour: i64, min: i64) -> Timestamp {
        // 2026-09-29T00:00:00Z plus the time of day.
        Timestamp::from_second(1_790_640_000 + hour * 3600 + min * 60).unwrap()
    }

    fn night() -> QuietHours {
        QuietHours {
            start_min: 22 * 60,
            end_min: 6 * 60,
        }
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        assert!(night().contains(at(23, 0)));
        assert!(night().contains(at(2, 30)));
        assert!(!night().contains(at(6, 0)), "the end is exclusive");
        assert!(!night().contains(at(12, 0)));
        assert!(night().contains(at(22, 0)));
    }

    #[test]
    fn same_day_quiet_hours() {
        let lunch = QuietHours {
            start_min: 12 * 60,
            end_min: 13 * 60,
        };
        assert!(lunch.contains(at(12, 30)));
        assert!(!lunch.contains(at(13, 0)));
        assert_eq!(lunch.next_open(at(12, 30)), at(13, 0));
    }

    #[test]
    fn held_alerts_go_out_when_the_window_ends() {
        assert_eq!(night().next_open(at(23, 15)), at(30, 0));
        assert_eq!(night().next_open(at(2, 15)), at(6, 0));
        assert_eq!(night().next_open(at(9, 0)), at(9, 0));
    }

    #[test]
    fn default_routing_sends_everything_at_once() {
        let routing = Routing::default();
        for event in [
            AlertEvent::WentDown,
            AlertEvent::Recovered,
            AlertEvent::Resend,
            AlertEvent::CertExpiring,
        ] {
            assert_eq!(routing.send_at(event, at(3, 0)), Some(at(3, 0)));
        }
        assert!(routing.is_default());
    }

    #[test]
    fn escalation_delays_only_the_down_alert() {
        let routing = Routing {
            escalate_after_mins: 10,
            ..Routing::default()
        };
        assert_eq!(
            routing.send_at(AlertEvent::WentDown, at(3, 0)),
            Some(at(3, 10))
        );
        assert_eq!(
            routing.send_at(AlertEvent::Recovered, at(3, 0)),
            Some(at(3, 0))
        );
        assert!(routing.escalates());
    }

    #[test]
    fn muted_events_are_dropped_and_tests_bypass_everything() {
        let routing = Routing {
            mute: vec![AlertEvent::Recovered, AlertEvent::Test],
            quiet: Some(night()),
            ..Routing::default()
        };
        assert_eq!(routing.send_at(AlertEvent::Recovered, at(12, 0)), None);
        assert_eq!(
            routing.send_at(AlertEvent::Test, at(23, 0)),
            Some(at(23, 0))
        );
        assert_eq!(
            routing.send_at(AlertEvent::WentDown, at(23, 0)),
            Some(at(30, 0))
        );
    }

    #[test]
    fn escalation_and_quiet_hours_combine() {
        let routing = Routing {
            escalate_after_mins: 30,
            quiet: Some(night()),
            ..Routing::default()
        };
        // Down at 21:45: due 22:15, inside quiet hours, held until 06:00.
        assert_eq!(
            routing.send_at(AlertEvent::WentDown, at(21, 45)),
            Some(at(30, 0))
        );
    }

    #[test]
    fn validation() {
        assert_eq!(Routing::default().validate(), Ok(()));
        let long = Routing {
            escalate_after_mins: 1441,
            ..Routing::default()
        };
        assert_eq!(long.validate(), Err(RoutingError::EscalateTooLong));
        let empty = Routing {
            quiet: Some(QuietHours {
                start_min: 60,
                end_min: 60,
            }),
            ..Routing::default()
        };
        assert_eq!(empty.validate(), Err(RoutingError::BadQuietHours));
        let mute_down = Routing {
            mute: vec![AlertEvent::WentDown],
            ..Routing::default()
        };
        assert_eq!(mute_down.validate(), Err(RoutingError::CannotMuteDown));
    }
}
