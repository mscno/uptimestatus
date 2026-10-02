//! Certificate expiry warnings: one alert as each threshold is crossed.

use jiff::Timestamp;

/// Thresholds (days left) at which to warn, most distant first: the
/// configured lead time, then 7, 3 and 1 day(s).
fn thresholds(warn_days: u32) -> Vec<u32> {
    let mut days = vec![warn_days, 7, 3, 1];
    days.retain(|d| *d <= warn_days && *d > 0);
    days.sort_unstable_by(|a, b| b.cmp(a));
    days.dedup();
    days
}

/// Whole days until `expires_at` (negative once expired).
pub fn days_left(expires_at: Timestamp, now: Timestamp) -> i64 {
    expires_at.duration_since(now).as_secs().div_euclid(86_400)
}

/// What to do about a certificate: `warned` is the threshold already warned
/// about (if any). Returns the threshold to remember, and whether to alert now.
pub fn warning(
    expires_at: Timestamp,
    now: Timestamp,
    warn_days: u32,
    warned: Option<u32>,
) -> (Option<u32>, bool) {
    let left = days_left(expires_at, now);
    let reached = thresholds(warn_days)
        .into_iter()
        .filter(|t| left <= i64::from(*t))
        .min();
    match reached {
        // Renewed (or never close): forget earlier warnings.
        None => (None, false),
        Some(threshold) => {
            let alert = warned.is_none_or(|w| threshold < w);
            (Some(threshold.min(warned.unwrap_or(u32::MAX))), alert)
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use pretty_assertions::assert_eq;

    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    fn in_days(days: i64) -> Timestamp {
        now() + SignedDuration::from_hours(days * 24) + SignedDuration::from_secs(60)
    }

    #[test]
    fn a_distant_expiry_is_quiet() {
        assert_eq!(warning(in_days(60), now(), 14, None), (None, false));
    }

    #[test]
    fn each_threshold_alerts_once() {
        assert_eq!(warning(in_days(14), now(), 14, None), (Some(14), true));
        assert_eq!(warning(in_days(12), now(), 14, Some(14)), (Some(14), false));
        assert_eq!(warning(in_days(7), now(), 14, Some(14)), (Some(7), true));
        assert_eq!(warning(in_days(5), now(), 14, Some(7)), (Some(7), false));
        assert_eq!(warning(in_days(1), now(), 14, Some(7)), (Some(1), true));
        assert_eq!(warning(in_days(-2), now(), 14, Some(1)), (Some(1), false));
    }

    #[test]
    fn a_renewed_certificate_resets_the_warnings() {
        assert_eq!(warning(in_days(90), now(), 14, Some(3)), (None, false));
    }

    #[test]
    fn warnings_can_be_turned_off() {
        assert_eq!(warning(in_days(1), now(), 0, None), (None, false));
    }

    #[test]
    fn short_lead_times_skip_larger_thresholds() {
        assert_eq!(warning(in_days(5), now(), 5, None), (Some(5), true));
        assert_eq!(warning(in_days(3), now(), 5, Some(5)), (Some(3), true));
    }
}
