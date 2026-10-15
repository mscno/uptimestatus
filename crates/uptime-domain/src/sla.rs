//! Monthly SLA reports: calendar months (UTC), the uptime a page promises,
//! and how a component's month becomes uptime, downtime and maintenance.
//!
//! A component's month is the sum of its daily tallies (checks for monitors,
//! seconds for manual statuses and overrides; see [`crate::component`]).
//! Uptime is [`Tally::uptime`]: maintenance is excluded and only DOWN counts
//! against it. Downtime and maintenance time are the observed time (the days
//! with data, today up to now) in proportion to DOWN and maintenance in the
//! tally: exact for manual statuses, estimated from the check interval for
//! monitors.

use std::{fmt, str::FromStr};

use jiff::{Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};

use crate::Tally;

/// A calendar month, in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    first: Date,
}

impl Month {
    pub fn new(year: i16, month: i8) -> Option<Self> {
        let first = Date::new(year, month, 1).ok()?;
        // Neighbours and the end must exist too.
        first
            .checked_add(1.month())
            .ok()?
            .checked_add(1.month())
            .ok()?;
        first.checked_sub(1.month()).ok()?;
        Some(Self { first })
    }

    /// The month containing `at`.
    pub fn of(at: Timestamp) -> Self {
        Self {
            first: at.to_zoned(TimeZone::UTC).date().first_of_month(),
        }
    }

    pub fn first_day(self) -> Date {
        self.first
    }

    pub fn last_day(self) -> Date {
        self.first.last_of_month()
    }

    /// The first instant of the month.
    pub fn start(self) -> Timestamp {
        utc_midnight(self.first)
    }

    /// The first instant after the month.
    pub fn end(self) -> Timestamp {
        utc_midnight(self.first.saturating_add(1.month()))
    }

    pub fn previous(self) -> Self {
        Self {
            first: self.first.saturating_sub(1.month()),
        }
    }

    pub fn next(self) -> Self {
        Self {
            first: self.first.saturating_add(1.month()),
        }
    }

    /// "September 2026".
    pub fn label(self) -> String {
        self.first.strftime("%B %Y").to_string()
    }

    /// Every day of the month, oldest first.
    pub fn days(self) -> Vec<Date> {
        let mut days = Vec::new();
        let mut day = self.first;
        while day.first_of_month() == self.first {
            days.push(day);
            match day.checked_add(1.day()) {
                Ok(next) => day = next,
                Err(_) => break,
            }
        }
        days
    }

    /// Whether `now` is inside the month (the report is month to date).
    pub fn contains(self, now: Timestamp) -> bool {
        self.start() <= now && now < self.end()
    }
}

fn utc_midnight(day: Date) -> Timestamp {
    day.to_zoned(TimeZone::UTC)
        .map_or(Timestamp::UNIX_EPOCH, |zoned| zoned.timestamp())
}

/// `2026-09`.
impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.first.strftime("%Y-%m"))
    }
}

impl FromStr for Month {
    type Err = String;

    /// `2026-09`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("a month is written like 2026-09: `{s}`");
        let (year, month) = s.trim().split_once('-').ok_or_else(invalid)?;
        if year.len() != 4 || month.len() != 2 {
            return Err(invalid());
        }
        let (Ok(year), Ok(month)) = (year.parse(), month.parse()) else {
            return Err(invalid());
        };
        Self::new(year, month).ok_or_else(invalid)
    }
}

impl Serialize for Month {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Month {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The uptime a status page promises, in thousandths of a percent
/// (99.9% is `99_900`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlaTarget(u32);

impl SlaTarget {
    /// The target as a fraction, e.g. `0.999`.
    pub fn ratio(self) -> f64 {
        f64::from(self.0) / 100_000.0
    }

    /// Whether `uptime` (a fraction) meets the target.
    pub fn met(self, uptime: f64) -> bool {
        // Compare in thousandths of a percent, so 99.9% meets 99.9%.
        (uptime * 100_000.0 + 1e-6).floor() >= f64::from(self.0)
    }
}

/// `99.9%`, `99.95%`, `100%`.
impl fmt::Display for SlaTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 1000;
        let fraction = self.0 % 1000;
        if fraction == 0 {
            write!(f, "{whole}%")
        } else {
            let digits = format!("{fraction:03}");
            write!(f, "{whole}.{}%", digits.trim_end_matches('0'))
        }
    }
}

impl FromStr for SlaTarget {
    type Err = String;

    /// `99.9`, `99.95%` or `100`: above 0, at most 100, three decimals.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!("an uptime target is a percentage such as 99.9, above 0 and at most 100: `{s}`")
        };
        let text = s.trim().trim_end_matches('%').trim();
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || whole.len() > 3
            || fraction.len() > 3
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(invalid());
        }
        let whole: u32 = whole.parse().map_err(|_| invalid())?;
        let fraction: u32 = format!("{fraction:0<3}").parse().map_err(|_| invalid())?;
        let value = whole * 1000 + fraction;
        if value == 0 || value > 100_000 {
            return Err(invalid());
        }
        Ok(Self(value))
    }
}

impl Serialize for SlaTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.to_string().trim_end_matches('%'))
    }
}

impl<'de> Deserialize<'de> for SlaTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A component's month in figures.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MonthFigures {
    /// `None` without anything to count.
    pub uptime: Option<f64>,
    pub downtime_secs: u64,
    pub maintenance_secs: u64,
    /// The time the figures cover.
    pub observed_secs: u64,
}

/// Figures for a month whose tallies sum to `tally`, over `observed_secs`
/// (see the module docs).
pub fn month_figures(tally: &Tally, observed_secs: u64) -> MonthFigures {
    let share = |count: u64| {
        if tally.total == 0 {
            0
        } else {
            u64::try_from(u128::from(observed_secs) * u128::from(count) / u128::from(tally.total))
                .unwrap_or(u64::MAX)
        }
    };
    MonthFigures {
        uptime: tally.uptime(),
        downtime_secs: share(tally.down),
        maintenance_secs: share(tally.maintenance),
        observed_secs,
    }
}

/// The page's figure: the mean of its components' uptimes (each component
/// counts the same, whatever its check interval).
pub fn overall_uptime(uptimes: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    let (sum, count) = uptimes
        .into_iter()
        .flatten()
        .fold((0.0, 0u32), |(sum, count), uptime| {
            (sum + uptime, count + 1)
        });
    (count > 0).then(|| sum / f64::from(count))
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn months_know_their_bounds_and_neighbours() {
        let month: Month = "2026-09".parse().unwrap();
        assert_eq!(month.to_string(), "2026-09");
        assert_eq!(month.label(), "September 2026");
        assert_eq!(month.first_day(), date(2026, 9, 1));
        assert_eq!(month.last_day(), date(2026, 9, 30));
        assert_eq!(month.days().len(), 30);
        assert_eq!(
            month.start(),
            "2026-09-01T00:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            month.end(),
            "2026-10-01T00:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(month.previous().to_string(), "2026-08");
        assert_eq!(month.next().to_string(), "2026-10");
        assert_eq!(
            Month::of("2026-12-31T23:59:59Z".parse().unwrap()).to_string(),
            "2026-12"
        );
        assert!(month.contains("2026-09-30T23:59:59Z".parse().unwrap()));
        assert!(!month.contains("2026-10-01T00:00:00Z".parse().unwrap()));
        assert_eq!("2028-02".parse::<Month>().unwrap().days().len(), 29);
    }

    #[rstest]
    #[case("2026-9")]
    #[case("2026-13")]
    #[case("26-09")]
    #[case("soon")]
    #[case("9999-12")]
    fn malformed_months_are_rejected(#[case] text: &str) {
        assert!(text.parse::<Month>().is_err());
    }

    #[rstest]
    #[case("99.9", 99_900, "99.9%")]
    #[case("99.95%", 99_950, "99.95%")]
    #[case(" 100 ", 100_000, "100%")]
    #[case("99.999", 99_999, "99.999%")]
    #[case("95", 95_000, "95%")]
    fn targets_parse_and_print(#[case] text: &str, #[case] value: u32, #[case] shown: &str) {
        let target: SlaTarget = text.parse().unwrap();
        assert_eq!(target, SlaTarget(value));
        assert_eq!(target.to_string(), shown);
    }

    #[rstest]
    #[case("0")]
    #[case("100.1")]
    #[case("99.9999")]
    #[case("-1")]
    #[case("abc")]
    #[case("")]
    #[case(".9")]
    fn bad_targets_are_rejected(#[case] text: &str) {
        assert!(text.parse::<SlaTarget>().is_err());
    }

    #[test]
    fn targets_are_met_at_or_above() {
        let target: SlaTarget = "99.9".parse().unwrap();
        assert!(target.met(0.999));
        assert!(target.met(1.0));
        assert!(!target.met(0.99899));
        assert_eq!(target.ratio(), 0.999);
    }

    #[test]
    fn figures_share_observed_time_by_the_tally() {
        // 1000 checks over 10 days: 10 down, 100 in maintenance.
        let tally = Tally {
            total: 1000,
            up: 890,
            down: 10,
            maintenance: 100,
            ..Tally::default()
        };
        let figures = month_figures(&tally, 864_000);
        assert_eq!(figures.downtime_secs, 8_640);
        assert_eq!(figures.maintenance_secs, 86_400);
        assert_eq!(figures.uptime, Some(890.0 / 900.0));
        assert_eq!(month_figures(&Tally::default(), 100).uptime, None);
        assert_eq!(month_figures(&Tally::default(), 100).downtime_secs, 0);
    }

    #[test]
    fn the_overall_figure_averages_components() {
        assert_eq!(overall_uptime([Some(1.0), Some(0.98), None]), Some(0.99));
        assert_eq!(overall_uptime([None]), None);
    }
}
