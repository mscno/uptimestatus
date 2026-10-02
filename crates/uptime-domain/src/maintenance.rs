//! Maintenance windows: planned time ranges during which monitors report
//! MAINTENANCE instead of failing, and alert nobody.

use std::{fmt, str::FromStr};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::MonitorKey;

/// The longest window accepted (a typo in the end date should not silence
/// monitors for a year).
pub const MAX_WINDOW: SignedDuration = SignedDuration::from_hours(14 * 24);

/// How often a window comes back, at the same UTC time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Daily,
    Weekly,
}

impl Repeat {
    pub const ALL: &'static [Self] = &[Self::Daily, Self::Weekly];

    pub const fn period(self) -> SignedDuration {
        match self {
            Self::Daily => SignedDuration::from_hours(24),
            Self::Weekly => SignedDuration::from_hours(7 * 24),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Daily => "Every day",
            Self::Weekly => "Every week",
        }
    }
}

impl fmt::Display for Repeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Repeat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| format!("unknown repeat `{s}`"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceSpec {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    /// Recurs after `starts_at`..`ends_at`: the window shifts by one period
    /// each time. `None` is a one-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Repeat>,
    /// The last moment a recurring window may start (inclusive); forever when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_until: Option<Timestamp>,
    /// Affected monitors.
    pub monitors: Vec<MonitorKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MaintenanceError {
    #[error("Give the maintenance a title.")]
    MissingTitle,
    #[error("The end must be after the start.")]
    EndsBeforeStart,
    #[error("A maintenance window can last at most 14 days.")]
    TooLong,
    #[error("Choose at least one affected monitor.")]
    NoMonitors,
    #[error("A repeating window must be shorter than its repeat interval.")]
    LongerThanRepeat,
    #[error("Repeat until must be after the first start.")]
    RepeatEndsEarly,
}

impl MaintenanceError {
    /// The form field the problem belongs to.
    pub fn field(&self) -> &'static str {
        match self {
            Self::MissingTitle => "title",
            Self::EndsBeforeStart | Self::TooLong => "ends_at",
            Self::NoMonitors => "monitors",
            Self::LongerThanRepeat => "repeat",
            Self::RepeatEndsEarly => "repeat_until",
        }
    }
}

impl MaintenanceSpec {
    pub fn validate(&self) -> Result<(), MaintenanceError> {
        if self.title.trim().is_empty() {
            return Err(MaintenanceError::MissingTitle);
        }
        if self.ends_at <= self.starts_at {
            return Err(MaintenanceError::EndsBeforeStart);
        }
        if self.ends_at.duration_since(self.starts_at) > MAX_WINDOW {
            return Err(MaintenanceError::TooLong);
        }
        if self.monitors.is_empty() {
            return Err(MaintenanceError::NoMonitors);
        }
        if let Some(repeat) = self.repeat {
            if self.ends_at.duration_since(self.starts_at) >= repeat.period() {
                return Err(MaintenanceError::LongerThanRepeat);
            }
            if self
                .repeat_until
                .is_some_and(|until| until < self.starts_at)
            {
                return Err(MaintenanceError::RepeatEndsEarly);
            }
        }
        Ok(())
    }

    /// Whether some occurrence covers `at` (start inclusive, end exclusive).
    pub fn is_active(&self, at: Timestamp) -> bool {
        let next = at.checked_add(SignedDuration::from_secs(1)).unwrap_or(at);
        self.occurrences(at, next)
            .iter()
            .any(|(start, end)| *start <= at && at < *end)
    }

    /// The `(start, end)` of every occurrence overlapping `[from, to)`,
    /// soonest first (at most [`MAX_OCCURRENCES`]). A one-off yields itself
    /// when it overlaps.
    pub fn occurrences(&self, from: Timestamp, to: Timestamp) -> Vec<(Timestamp, Timestamp)> {
        let length = self.ends_at.duration_since(self.starts_at);
        let overlaps = |start: Timestamp, end: Timestamp| start < to && end > from;
        let Some(repeat) = self.repeat else {
            return if overlaps(self.starts_at, self.ends_at) {
                vec![(self.starts_at, self.ends_at)]
            } else {
                Vec::new()
            };
        };
        let period = repeat.period();
        // The first occurrence that could still overlap `from`.
        let behind = from.duration_since(self.starts_at) - length;
        let first = (behind.as_secs() / period.as_secs()).max(0);
        let mut found = Vec::new();
        for n in first.. {
            let Some(start) = i32::try_from(n)
                .ok()
                .and_then(|n| self.starts_at.checked_add(period * n).ok())
            else {
                break;
            };
            let Ok(end) = start.checked_add(length) else {
                break;
            };
            if start >= to || self.repeat_until.is_some_and(|until| start > until) {
                break;
            }
            if overlaps(start, end) {
                found.push((start, end));
                if found.len() >= MAX_OCCURRENCES {
                    break;
                }
            }
        }
        found
    }
}

/// Most occurrences [`MaintenanceSpec::occurrences`] returns.
pub const MAX_OCCURRENCES: usize = 400;

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn t(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn spec(starts: i64, ends: i64) -> MaintenanceSpec {
        MaintenanceSpec {
            title: "Database upgrade".into(),
            description: None,
            starts_at: t(starts),
            ends_at: t(ends),
            repeat: None,
            repeat_until: None,
            monitors: vec!["db".parse().unwrap()],
        }
    }

    #[test]
    fn a_window_is_active_from_its_start_until_its_end() {
        let window = spec(100, 200);
        assert!(!window.is_active(t(99)));
        assert!(window.is_active(t(100)));
        assert!(window.is_active(t(199)));
        assert!(!window.is_active(t(200)));
    }

    #[test]
    fn valid_windows_pass() {
        assert_eq!(spec(0, 3600).validate(), Ok(()));
    }

    #[test]
    fn windows_are_checked() {
        assert_eq!(
            spec(10, 10).validate(),
            Err(MaintenanceError::EndsBeforeStart)
        );
        assert_eq!(
            spec(0, 15 * 24 * 3600).validate(),
            Err(MaintenanceError::TooLong)
        );
        let untitled = MaintenanceSpec {
            title: " ".into(),
            ..spec(0, 60)
        };
        assert_eq!(untitled.validate(), Err(MaintenanceError::MissingTitle));
        let empty = MaintenanceSpec {
            monitors: vec![],
            ..spec(0, 60)
        };
        assert_eq!(empty.validate().unwrap_err().field(), "monitors");
    }

    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;

    fn weekly(starts: i64, hours: i64) -> MaintenanceSpec {
        MaintenanceSpec {
            repeat: Some(Repeat::Weekly),
            ..spec(starts, starts + hours * HOUR)
        }
    }

    #[test]
    fn a_weekly_window_comes_back_every_week() {
        let window = weekly(2 * DAY, 2);
        assert!(window.is_active(t(2 * DAY + HOUR)));
        assert!(!window.is_active(t(3 * DAY)));
        assert!(window.is_active(t(9 * DAY + HOUR)));
        assert!(window.is_active(t(2 * DAY + 70 * 7 * DAY + HOUR)));
        assert!(!window.is_active(t(9 * DAY + 3 * HOUR)));
        assert!(!window.is_active(t(DAY)), "not before the first one");
    }

    #[test]
    fn repeat_until_stops_the_series() {
        let window = MaintenanceSpec {
            repeat_until: Some(t(9 * DAY)),
            ..weekly(2 * DAY, 2)
        };
        assert!(window.is_active(t(9 * DAY + HOUR)));
        assert!(!window.is_active(t(16 * DAY + HOUR)));
    }

    #[test]
    fn occurrences_are_listed_within_a_range() {
        let window = MaintenanceSpec {
            repeat: Some(Repeat::Daily),
            ..spec(HOUR, 2 * HOUR)
        };
        let got: Vec<_> = window
            .occurrences(t(DAY), t(3 * DAY + 2 * HOUR))
            .into_iter()
            .map(|(start, _)| start)
            .collect();
        assert_eq!(got, [t(DAY + HOUR), t(2 * DAY + HOUR), t(3 * DAY + HOUR)]);
        let once = spec(HOUR, 2 * HOUR).occurrences(t(0), t(DAY));
        assert_eq!(once, [(t(HOUR), t(2 * HOUR))]);
        assert_eq!(
            spec(HOUR, 2 * HOUR).occurrences(t(DAY), t(2 * DAY)).len(),
            0
        );
    }

    #[test]
    fn repeating_windows_must_fit_their_interval() {
        let long = MaintenanceSpec {
            repeat: Some(Repeat::Daily),
            ..spec(0, 25 * HOUR)
        };
        assert_eq!(long.validate(), Err(MaintenanceError::LongerThanRepeat));
        let early = MaintenanceSpec {
            repeat_until: Some(t(-1)),
            ..weekly(0, 1)
        };
        assert_eq!(early.validate(), Err(MaintenanceError::RepeatEndsEarly));
        assert_eq!(weekly(0, 3).validate(), Ok(()));
    }
}
