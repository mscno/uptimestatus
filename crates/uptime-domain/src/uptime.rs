//! Counting check outcomes into uptime percentages.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};

use crate::MonitorState;

/// Per-state check counts for a period (a day, an hour, ...).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub total: u64,
    pub up: u64,
    pub degraded: u64,
    pub pending: u64,
    pub down: u64,
    pub maintenance: u64,
}

impl Tally {
    /// Counts one check that left the monitor in `state`.
    ///
    /// `Unknown` and `Paused` never result from a check and are ignored.
    pub fn record(&mut self, state: MonitorState) {
        let bucket = match state {
            MonitorState::Up => &mut self.up,
            MonitorState::Degraded => &mut self.degraded,
            MonitorState::Pending => &mut self.pending,
            MonitorState::Down => &mut self.down,
            MonitorState::Maintenance => &mut self.maintenance,
            MonitorState::Unknown | MonitorState::Paused => return,
        };
        *bucket += 1;
        self.total += 1;
    }

    /// Adds another period's counts to this one.
    pub fn merge(&mut self, other: &Self) {
        self.total += other.total;
        self.up += other.up;
        self.degraded += other.degraded;
        self.pending += other.pending;
        self.down += other.down;
        self.maintenance += other.maintenance;
    }

    /// Fraction of countable checks that were not DOWN, in `0.0..=1.0`.
    ///
    /// PENDING counts as up (the failure was never confirmed); maintenance is
    /// excluded. Returns `None` when there is nothing to count.
    #[allow(clippy::cast_precision_loss)] // counts stay far below 2^52
    pub fn uptime(&self) -> Option<f64> {
        let countable = self.total.saturating_sub(self.maintenance);
        (countable > 0).then(|| (self.up + self.degraded + self.pending) as f64 / countable as f64)
    }
}

/// Uptime over the trailing 24 hours, 7, 30 and 90 days.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UptimeWindows {
    pub h24: Option<f64>,
    pub d7: Option<f64>,
    pub d30: Option<f64>,
    pub d90: Option<f64>,
}

impl UptimeWindows {
    /// The 24-hour figure comes from raw checks (`recent`: state after each
    /// check); the day windows sum the UTC-day tallies of the last N days,
    /// today included.
    pub fn compute(
        now: Timestamp,
        recent: &[(Timestamp, MonitorState)],
        days: &BTreeMap<Date, Tally>,
    ) -> Self {
        let since = now
            .checked_sub(SignedDuration::from_hours(24))
            .unwrap_or(now);
        let mut day = Tally::default();
        for (at, state) in recent {
            if *at >= since {
                day.record(*state);
            }
        }
        let today = now.to_zoned(TimeZone::UTC).date();
        let over = |n: i64| {
            let first = today.checked_sub((n - 1).days()).unwrap_or(today);
            let mut total = Tally::default();
            for (_, tally) in days.range(first..=today) {
                total.merge(tally);
            }
            total.uptime()
        };
        Self {
            h24: day.uptime(),
            d7: over(7),
            d30: over(30),
            d90: over(90),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn empty_tally_has_no_uptime() {
        assert_eq!(Tally::default().uptime(), None);
    }

    #[test]
    fn records_each_state_in_its_bucket() {
        let mut tally = Tally::default();
        for state in [
            MonitorState::Up,
            MonitorState::Up,
            MonitorState::Degraded,
            MonitorState::Pending,
            MonitorState::Down,
            MonitorState::Maintenance,
        ] {
            tally.record(state);
        }
        assert_eq!(
            tally,
            Tally {
                total: 6,
                up: 2,
                degraded: 1,
                pending: 1,
                down: 1,
                maintenance: 1
            }
        );
    }

    #[test]
    fn ignores_states_that_are_not_check_results() {
        let mut tally = Tally::default();
        tally.record(MonitorState::Unknown);
        tally.record(MonitorState::Paused);
        assert_eq!(tally, Tally::default());
    }

    #[test]
    fn uptime_counts_pending_as_up_and_excludes_maintenance() {
        let tally = Tally {
            total: 10,
            up: 6,
            degraded: 1,
            pending: 1,
            down: 1,
            maintenance: 1,
        };
        // (6 + 1 + 1) / (10 - 1) = 8 / 9
        assert_eq!(tally.uptime(), Some(8.0 / 9.0));
    }

    #[test]
    fn only_maintenance_has_no_uptime() {
        let tally = Tally {
            total: 3,
            maintenance: 3,
            ..Default::default()
        };
        assert_eq!(tally.uptime(), None);
    }

    #[test]
    fn merge_adds_all_buckets() {
        let mut a = Tally {
            total: 2,
            up: 1,
            down: 1,
            ..Default::default()
        };
        let b = Tally {
            total: 3,
            up: 1,
            degraded: 1,
            maintenance: 1,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(
            a,
            Tally {
                total: 5,
                up: 2,
                degraded: 1,
                down: 1,
                maintenance: 1,
                pending: 0
            }
        );
    }

    #[test]
    fn windows_combine_raw_checks_and_daily_tallies() {
        use jiff::civil::date;
        let now = "2026-09-29T12:00:00Z".parse::<Timestamp>().unwrap();
        let hour = |h: i64| now.checked_sub(SignedDuration::from_hours(h)).unwrap();
        let recent = [
            (hour(30), MonitorState::Down), // outside 24h
            (hour(5), MonitorState::Up),
            (hour(1), MonitorState::Down),
        ];
        let tally = |up, down| Tally {
            total: up + down,
            up,
            down,
            ..Tally::default()
        };
        let days = BTreeMap::from([
            (date(2026, 9, 29), tally(9, 1)),
            (date(2026, 9, 25), tally(10, 0)),
            (date(2026, 9, 1), tally(0, 10)),
        ]);

        let windows = UptimeWindows::compute(now, &recent, &days);

        assert_eq!(windows.h24, Some(0.5));
        assert_eq!(windows.d7, Some(19.0 / 20.0));
        assert_eq!(windows.d30, Some(19.0 / 30.0));
        assert_eq!(windows.d90, windows.d30);
        assert_eq!(
            UptimeWindows::compute(now, &[], &BTreeMap::new()),
            UptimeWindows::default()
        );
    }
}
