//! Scheduling arithmetic.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use uptime_domain::MonitorKey;

/// When the next check is due after the slot `scheduled_for`.
///
/// Drift-free: slots stay on a fixed cadence (`scheduled_for + next_in`) no
/// matter how long a check took. If that moment has already passed (the
/// scheduler was down or far behind), the overdue slot just ran, so the next
/// one is a full `next_in` from now rather than immediately again.
pub fn next_run_at(scheduled_for: Timestamp, next_in: Duration, now: Timestamp) -> Timestamp {
    let step = signed(next_in);
    match scheduled_for.saturating_add(step) {
        Ok(next) if next > now => next,
        _ => now.saturating_add(step).unwrap_or(now),
    }
}

/// When a new monitor's first check is due: soon, but spread out so monitors
/// created together (e.g. by a seed file) don't all fire in the same second.
///
/// The offset is deterministic per key and below `min(interval, 60s)`.
pub fn first_run_at(now: Timestamp, key: &MonitorKey, interval: Duration) -> Timestamp {
    let window_ms = interval.min(Duration::from_secs(60)).as_millis().max(1);
    let offset_ms = u128::from(fnv1a(key.as_str())) % window_ms;
    let offset = Duration::from_millis(u64::try_from(offset_ms).unwrap_or(0));
    now.saturating_add(signed(offset)).unwrap_or(now)
}

/// Stable 64-bit FNV-1a hash (std's hasher is not stable across releases).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn signed(duration: Duration) -> SignedDuration {
    SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn t(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn next_slot_follows_the_previous_slot_not_the_check_duration() {
        // Scheduled at 0, the check finished at 7s: next slot is still 60s.
        assert_eq!(next_run_at(t(0), secs(60), t(7)), t(60));
    }

    #[test]
    fn retry_cadence_is_used_when_given() {
        assert_eq!(next_run_at(t(0), secs(20), t(3)), t(20));
    }

    #[test]
    fn after_falling_behind_the_next_slot_is_a_full_interval_out() {
        // The scheduler was down for 5 minutes and just ran the overdue slot:
        // the next check is one interval from now, not immediately again.
        assert_eq!(next_run_at(t(0), secs(60), t(300)), t(360));
    }

    #[test]
    fn a_slow_check_does_not_push_the_cadence() {
        // Scheduled at 0, took 59s: the next slot is still 60.
        assert_eq!(next_run_at(t(0), secs(60), t(59)), t(60));
    }

    #[test]
    fn first_run_is_spread_below_the_interval_and_stable_per_key() {
        let key: MonitorKey = "api".parse().unwrap();
        let first = first_run_at(t(0), &key, secs(30));
        assert!(first >= t(0) && first < t(30), "{first}");
        assert_eq!(first_run_at(t(0), &key, secs(30)), first, "deterministic");
    }

    #[test]
    fn first_run_offset_is_capped_at_a_minute() {
        for key in ["a", "b", "c", "api", "web", "db-1", "db-2"] {
            let first = first_run_at(t(0), &key.parse().unwrap(), secs(3600));
            assert!(first < t(60), "{key}: {first}");
        }
    }

    #[test]
    fn different_keys_spread_out() {
        let offsets: std::collections::HashSet<_> = (0..20)
            .map(|i| first_run_at(t(0), &format!("m{i}").parse().unwrap(), secs(60)))
            .collect();
        assert!(
            offsets.len() > 10,
            "keys should not collide much: {}",
            offsets.len()
        );
    }
}
