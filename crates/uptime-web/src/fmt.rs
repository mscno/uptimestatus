//! Human-friendly formatting for the UI.

use std::time::Duration;

use jiff::Timestamp;

/// "just now", "42s ago", "5m ago", "3h ago", "2d ago" (or "in 30s" for the future).
pub fn relative(at: Timestamp, now: Timestamp) -> String {
    let seconds = now.duration_since(at).as_secs();
    let magnitude = seconds.unsigned_abs();
    if magnitude < 5 {
        return "just now".to_owned();
    }
    let amount = match magnitude {
        0..60 => format!("{magnitude}s"),
        60..3600 => format!("{}m", magnitude / 60),
        3600..86_400 => format!("{}h", magnitude / 3600),
        _ => format!("{}d", magnitude / 86_400),
    };
    if seconds >= 0 {
        format!("{amount} ago")
    } else {
        format!("in {amount}")
    }
}

/// A calendar time in UTC: "Mar 1, 22:00 UTC".
pub fn when(at: Timestamp) -> String {
    at.strftime("%b %-d, %H:%M UTC").to_string()
}

/// A calendar time with the year, in UTC: "Mar 1, 2031, 22:00 UTC".
pub fn when_dated(at: Timestamp) -> String {
    at.strftime("%b %-d, %Y, %H:%M UTC").to_string()
}

/// `datetime-local` input value (UTC): "2031-03-01T22:00".
pub fn datetime_local(at: Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M").to_string()
}

/// Parses a `datetime-local` value as UTC.
pub fn parse_datetime_local(text: &str) -> Option<Timestamp> {
    let civil: jiff::civil::DateTime = text.trim().parse().ok()?;
    civil
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

/// A compact duration: "250ms", "10s", "1m", "1m 30s", "2h", "14d".
pub fn duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) && !duration.is_zero() {
        return format!("{}ms", duration.as_millis());
    }
    let mut remaining = duration.as_secs();
    let parts: Vec<String> = [(86_400, "d"), (3600, "h"), (60, "m"), (1, "s")]
        .into_iter()
        .filter_map(|(unit, suffix)| {
            let count = remaining / unit;
            remaining %= unit;
            (count > 0).then(|| format!("{count}{suffix}"))
        })
        .take(2)
        .collect();
    if parts.is_empty() {
        "0s".to_owned()
    } else {
        parts.join(" ")
    }
}

/// An uptime ratio as a percentage with two decimals, never rounding up to 100.
pub fn uptime(ratio: f64) -> String {
    // Floor, so 99.999% shows as 99.99% rather than a misleading 100.00%.
    let hundredths = (ratio.clamp(0.0, 1.0) * 10_000.0 + 1e-9).floor();
    format!("{:.2}%", hundredths / 100.0)
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    #[test]
    fn calendar_times_and_form_values() {
        let at: Timestamp = "2031-03-01T22:05:00Z".parse().unwrap();
        assert_eq!(when(at), "Mar 1, 22:05 UTC");
        assert_eq!(when_dated(at), "Mar 1, 2031, 22:05 UTC");
        assert_eq!(datetime_local(at), "2031-03-01T22:05");
        assert_eq!(parse_datetime_local("2031-03-01T22:05"), Some(at));
        assert_eq!(parse_datetime_local("tomorrow"), None);
    }

    #[rstest]
    #[case(0, "just now")]
    #[case(4, "just now")]
    #[case(5, "5s ago")]
    #[case(59, "59s ago")]
    #[case(60, "1m ago")]
    #[case(3599, "59m ago")]
    #[case(3600, "1h ago")]
    #[case(86_399, "23h ago")]
    #[case(86_400, "1d ago")]
    #[case(-30, "in 30s")]
    #[case(-3600, "in 1h")]
    fn relative_times(#[case] seconds_ago: i64, #[case] expected: &str) {
        let at = now() - SignedDuration::from_secs(seconds_ago);
        assert_eq!(relative(at, now()), expected);
    }

    #[rstest]
    #[case(Duration::from_millis(250), "250ms")]
    #[case(Duration::from_secs(10), "10s")]
    #[case(Duration::from_secs(60), "1m")]
    #[case(Duration::from_secs(90), "1m 30s")]
    #[case(Duration::from_secs(7200), "2h")]
    #[case(Duration::from_secs(14 * 86_400), "14d")]
    #[case(Duration::ZERO, "0s")]
    fn durations(#[case] duration: Duration, #[case] expected: &str) {
        assert_eq!(super::duration(duration), expected);
    }

    #[rstest]
    #[case(1.0, "100.00%")]
    #[case(0.999_999, "99.99%")]
    #[case(0.9876, "98.76%")]
    #[case(0.0, "0.00%")]
    fn uptime_percentages(#[case] ratio: f64, #[case] expected: &str) {
        assert_eq!(uptime(ratio), expected);
    }
}
