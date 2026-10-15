//! Statuses admins set by hand: manual components, and overrides of monitor
//! components. Their history becomes uptime the way checks do, measured in
//! seconds instead of checks:
//!
//! - operational counts as up, degraded as up (like slow checks), maintenance
//!   is excluded;
//! - a major outage counts as down;
//! - a partial outage counts half its time as down, half as degraded;
//! - a day with any degraded or partial-outage time shows at least a warning
//!   bar ([`component_bar_level`]).

use std::{collections::BTreeMap, fmt, str::FromStr};

use jiff::{SignedDuration, Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};

use crate::{BarLevel, PageStatus, Tally, bar_level};

/// A status an admin sets on a component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
}

/// Why a string is not a [`ComponentStatus`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "component status must be operational, degraded, partial_outage, major_outage or maintenance: `{0}`"
)]
pub struct UnknownComponentStatus(pub String);

impl ComponentStatus {
    pub const ALL: [Self; 5] = [
        Self::Operational,
        Self::Degraded,
        Self::PartialOutage,
        Self::MajorOutage,
        Self::Maintenance,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operational => "operational",
            Self::Degraded => "degraded",
            Self::PartialOutage => "partial_outage",
            Self::MajorOutage => "major_outage",
            Self::Maintenance => "maintenance",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Operational => "Operational",
            Self::Degraded => "Degraded",
            Self::PartialOutage => "Partial outage",
            Self::MajorOutage => "Major outage",
            Self::Maintenance => "Maintenance",
        }
    }

    /// What this component alone says about the page.
    pub fn page_status(self) -> PageStatus {
        match self {
            Self::Operational => PageStatus::Operational,
            Self::Degraded => PageStatus::Degraded,
            Self::PartialOutage => PageStatus::PartialOutage,
            Self::MajorOutage => PageStatus::MajorOutage,
            Self::Maintenance => PageStatus::Maintenance,
        }
    }

    /// Adds `secs` spent in this status to `tally` (see the module docs).
    pub fn record(self, tally: &mut Tally, secs: u64) {
        match self {
            Self::Operational => tally.up += secs,
            Self::Degraded => tally.degraded += secs,
            Self::PartialOutage => {
                tally.down += secs / 2;
                tally.degraded += secs - secs / 2;
            }
            Self::MajorOutage => tally.down += secs,
            Self::Maintenance => tally.maintenance += secs,
        }
        tally.total += secs;
    }
}

impl fmt::Display for ComponentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ComponentStatus {
    type Err = UnknownComponentStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s.trim())
            .ok_or_else(|| UnknownComponentStatus(s.to_owned()))
    }
}

/// From `at` on, a component had `status`; `None` means no manual status
/// (an override was cleared and the monitor speaks again).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusChange {
    pub at: Timestamp,
    pub status: Option<ComponentStatus>,
}

/// Seconds in each status within `[from, to)`, from changes sorted oldest
/// first. Time before the first change, and time without a manual status,
/// is not counted.
pub fn tally_between(changes: &[StatusChange], from: Timestamp, to: Timestamp) -> Tally {
    let mut tally = Tally::default();
    for (index, change) in changes.iter().enumerate() {
        let Some(status) = change.status else {
            continue;
        };
        let start = change.at.max(from);
        let end = changes.get(index + 1).map_or(to, |next| next.at.min(to));
        if end > start {
            let secs = u64::try_from(end.duration_since(start).as_secs()).unwrap_or(0);
            status.record(&mut tally, secs);
        }
    }
    tally
}

/// Per-UTC-day seconds for `first..=last`, counting up to `now` (today is
/// partial). Days without any manual status are left out.
pub fn daily_tallies(
    changes: &[StatusChange],
    first: Date,
    last: Date,
    now: Timestamp,
) -> BTreeMap<Date, Tally> {
    let mut days = BTreeMap::new();
    let mut day = first;
    while day <= last {
        let (Ok(start), Some(end)) = (
            day.to_zoned(TimeZone::UTC).map(|z| z.timestamp()),
            day.checked_add(1.day())
                .ok()
                .and_then(|next| next.to_zoned(TimeZone::UTC).ok())
                .map(|z| z.timestamp().min(now)),
        ) else {
            break;
        };
        let tally = tally_between(changes, start, end);
        if tally.total > 0 {
            days.insert(day, tally);
        }
        let Ok(next) = day.checked_add(1.day()) else {
            break;
        };
        day = next;
    }
    days
}

/// A monitor's day in seconds, with an override in force for part of it: the
/// override's seconds (`overlay`) replace that share of the monitor's checks,
/// which are scaled to the rest of `span_secs` (the part of the day observed).
/// Days are always in seconds, so they add up with fully overridden days.
pub fn blend(monitor: Option<&Tally>, overlay: &Tally, span_secs: u64) -> Option<Tally> {
    let rest = span_secs.saturating_sub(overlay.total);
    let mut out = *overlay;
    if let Some(monitor) = monitor.filter(|m| m.total > 0 && rest > 0) {
        let scale = |count: u64| {
            u64::try_from(u128::from(count) * u128::from(rest) / u128::from(monitor.total))
                .unwrap_or(u64::MAX)
        };
        let scaled = Tally {
            total: scale(monitor.up)
                + scale(monitor.degraded)
                + scale(monitor.pending)
                + scale(monitor.down)
                + scale(monitor.maintenance),
            up: scale(monitor.up),
            degraded: scale(monitor.degraded),
            pending: scale(monitor.pending),
            down: scale(monitor.down),
            maintenance: scale(monitor.maintenance),
        };
        out.merge(&scaled);
    }
    (out.total > 0).then_some(out)
}

/// A component's days for `first..=last`: a manual component's from its
/// history alone; a monitor component's from its check tallies (`monitor`),
/// with any override time blended in ([`blend`]).
pub fn component_days(
    monitor: Option<&BTreeMap<Date, Tally>>,
    changes: &[StatusChange],
    first: Date,
    last: Date,
    now: Timestamp,
) -> BTreeMap<Date, Tally> {
    let Some(monitor) = monitor else {
        return daily_tallies(changes, first, last, now);
    };
    if changes.iter().all(|c| c.status.is_none()) {
        return monitor
            .range(first..=last)
            .map(|(day, tally)| (*day, *tally))
            .collect();
    }
    let overrides = daily_tallies(changes, first, last, now);
    let mut days = BTreeMap::new();
    let mut day = first;
    while day <= last {
        let overlay = overrides.get(&day).copied().unwrap_or_default();
        if let Some(tally) = blend(monitor.get(&day), &overlay, day_span(day, now)) {
            days.insert(day, tally);
        }
        let Ok(next) = day.checked_add(1.day()) else {
            break;
        };
        day = next;
    }
    days
}

/// The last 24 hours of a component: a manual component's history, or a
/// monitor's checks (`checks`, the state after each) blended with override time.
pub fn component_last_day(
    checks: Option<&[(Timestamp, crate::MonitorState)]>,
    changes: &[StatusChange],
    now: Timestamp,
) -> Tally {
    let since = now
        .checked_sub(SignedDuration::from_hours(24))
        .unwrap_or(now);
    let overlay = tally_between(changes, since, now);
    let Some(checks) = checks else {
        return overlay;
    };
    let mut monitor = Tally::default();
    for (at, state) in checks {
        if *at >= since {
            monitor.record(*state);
        }
    }
    blend(Some(&monitor), &overlay, 24 * 3600).unwrap_or(monitor)
}

/// A manual or overridden component's bar: by uptime like any bar, but a day
/// with degraded time is at least a warning.
pub fn component_bar_level(tally: Option<&Tally>) -> BarLevel {
    match bar_level(tally) {
        BarLevel::Up if tally.is_some_and(|t| t.degraded > 0) => BarLevel::Warn,
        level => level,
    }
}

/// Seconds in a UTC day, or up to `now` for today.
pub fn day_span(day: Date, now: Timestamp) -> u64 {
    let Ok(start) = day.to_zoned(TimeZone::UTC).map(|z| z.timestamp()) else {
        return 0;
    };
    let end = start
        .checked_add(SignedDuration::from_hours(24))
        .map_or(now, |end| end.min(now));
    u64::try_from(end.duration_since(start).as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    fn t(hours: i64) -> Timestamp {
        // 2031-03-01T00:00:00Z plus `hours`.
        Timestamp::from_second(1_930_089_600 + hours * 3600).unwrap()
    }

    fn change(hours: i64, status: Option<ComponentStatus>) -> StatusChange {
        StatusChange {
            at: t(hours),
            status,
        }
    }

    #[test]
    fn statuses_parse_and_name_themselves() {
        for status in ComponentStatus::ALL {
            assert_eq!(status.as_str().parse::<ComponentStatus>(), Ok(status));
        }
        assert!("broken".parse::<ComponentStatus>().is_err());
        assert_eq!(
            serde_json::to_value(ComponentStatus::PartialOutage).unwrap(),
            "partial_outage"
        );
        assert_eq!(
            ComponentStatus::MajorOutage.page_status(),
            PageStatus::MajorOutage
        );
    }

    #[rstest]
    #[case(ComponentStatus::Operational, Tally { total: 100, up: 100, ..Tally::default() })]
    #[case(ComponentStatus::Degraded, Tally { total: 100, degraded: 100, ..Tally::default() })]
    #[case(ComponentStatus::PartialOutage, Tally { total: 100, degraded: 50, down: 50, ..Tally::default() })]
    #[case(ComponentStatus::MajorOutage, Tally { total: 100, down: 100, ..Tally::default() })]
    #[case(ComponentStatus::Maintenance, Tally { total: 100, maintenance: 100, ..Tally::default() })]
    fn each_status_counts_by_the_documented_rule(
        #[case] status: ComponentStatus,
        #[case] expected: Tally,
    ) {
        let mut tally = Tally::default();
        status.record(&mut tally, 100);
        assert_eq!(tally, expected);
    }

    #[test]
    fn partial_outages_count_half_as_down() {
        let mut tally = Tally::default();
        ComponentStatus::PartialOutage.record(&mut tally, 3600);
        ComponentStatus::Operational.record(&mut tally, 3600);
        assert_eq!(tally.uptime(), Some(0.75));
    }

    #[test]
    fn time_is_split_between_changes_and_clipped_to_the_range() {
        let changes = [
            change(0, Some(ComponentStatus::Operational)),
            change(2, Some(ComponentStatus::MajorOutage)),
            change(3, None),
            change(5, Some(ComponentStatus::Operational)),
        ];
        let tally = tally_between(&changes, t(1), t(6));
        // 1h operational, 1h major outage, 2h with no manual status, 1h operational.
        assert_eq!(
            tally,
            Tally {
                total: 3 * 3600,
                up: 2 * 3600,
                down: 3600,
                ..Tally::default()
            }
        );
        assert_eq!(tally_between(&changes, t(-5), t(0)), Tally::default());
    }

    #[test]
    fn daily_tallies_stop_at_now_and_skip_empty_days() {
        let changes = [
            change(24 + 12, Some(ComponentStatus::Operational)),
            change(24 + 18, Some(ComponentStatus::Degraded)),
        ];
        let first = t(0).to_zoned(TimeZone::UTC).date();
        let days = daily_tallies(
            &changes,
            first,
            first.checked_add(2.days()).unwrap(),
            t(48 + 6),
        );
        assert_eq!(days.len(), 2, "day one had no manual status yet");
        let day_two = days[&first.checked_add(1.day()).unwrap()];
        assert_eq!((day_two.up, day_two.degraded), (6 * 3600, 6 * 3600));
        let today = days[&first.checked_add(2.days()).unwrap()];
        assert_eq!(today.total, 6 * 3600, "today counts up to now");
        assert_eq!(component_bar_level(Some(&day_two)), BarLevel::Warn);
    }

    #[test]
    fn overrides_replace_their_share_of_a_monitors_day() {
        let monitor = Tally {
            total: 100,
            up: 100,
            ..Tally::default()
        };
        // Six hours of a full day under a major-outage override.
        let mut overlay = Tally::default();
        ComponentStatus::MajorOutage.record(&mut overlay, 6 * 3600);
        let day = blend(Some(&monitor), &overlay, 24 * 3600).unwrap();
        assert_eq!(day.total, 24 * 3600);
        assert_eq!(day.uptime(), Some(0.75));
        // Without an override the day keeps its uptime, in seconds.
        let plain = blend(Some(&monitor), &Tally::default(), 86_400).unwrap();
        assert_eq!((plain.total, plain.uptime()), (86_400, Some(1.0)));
        assert_eq!(blend(None, &overlay, 86_400), Some(overlay));
        assert_eq!(blend(None, &Tally::default(), 86_400), None);
    }

    #[test]
    fn component_days_follow_history_or_blend_overrides_into_checks() {
        let first = t(0).to_zoned(TimeZone::UTC).date();
        let second = first.checked_add(1.day()).unwrap();
        let checks = |up, down| Tally {
            total: up + down,
            up,
            down,
            ..Tally::default()
        };
        let monitor: BTreeMap<Date, Tally> = [(first, checks(1440, 0)), (second, checks(1440, 0))]
            .into_iter()
            .collect();
        // No override history: the monitor's own tallies, as they are.
        assert_eq!(
            component_days(Some(&monitor), &[], first, second, t(48)),
            monitor
        );
        // A 12-hour major-outage override on day two.
        let changes = [
            change(24, Some(ComponentStatus::MajorOutage)),
            change(36, None),
        ];
        let days = component_days(Some(&monitor), &changes, first, second, t(48));
        assert_eq!(days[&first].uptime(), Some(1.0));
        assert_eq!(days[&first].total, 86_400, "every day in seconds");
        assert_eq!(days[&second].uptime(), Some(0.5));
        // A manual component: history alone.
        let manual = component_days(None, &changes, first, second, t(48));
        assert_eq!(manual.len(), 1);
        assert_eq!(manual[&second].down, 12 * 3600);
    }

    #[test]
    fn the_last_day_blends_checks_and_overrides() {
        let checks = [
            (t(30), crate::MonitorState::Up),
            (t(40), crate::MonitorState::Up),
        ];
        let changes = [change(36, Some(ComponentStatus::MajorOutage))];
        let day = component_last_day(Some(&checks), &changes, t(48));
        assert_eq!(day.uptime(), Some(0.5));
        let manual = component_last_day(None, &changes, t(48));
        assert_eq!(manual.uptime(), Some(0.0));
        assert_eq!(
            component_last_day(Some(&checks), &[], t(48)).uptime(),
            Some(1.0)
        );
    }

    #[test]
    fn degraded_days_are_at_least_a_warning() {
        let mut slow = Tally::default();
        ComponentStatus::Degraded.record(&mut slow, 60);
        ComponentStatus::Operational.record(&mut slow, 86_000);
        assert_eq!(bar_level(Some(&slow)), BarLevel::Up);
        assert_eq!(component_bar_level(Some(&slow)), BarLevel::Warn);
        assert_eq!(component_bar_level(None), BarLevel::None);
    }

    #[test]
    fn day_spans_end_at_now() {
        let day = t(0).to_zoned(TimeZone::UTC).date();
        assert_eq!(day_span(day, t(100)), 86_400);
        assert_eq!(day_span(day, t(3)), 3 * 3600);
    }
}
