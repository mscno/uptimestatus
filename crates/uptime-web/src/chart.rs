//! Server-rendered latency charts: an inline SVG with the median (p50) and
//! 95th percentile lines. Colors come from CSS (`.chart-p50`, `.chart-p95`).

use std::fmt::Write as _;

use uptime_domain::latency::Bucket;

const WIDTH: f64 = 600.0;
const HEIGHT: f64 = 120.0;
const PAD: f64 = 4.0;

/// Where values land in the drawing: the time axis spreads the buckets
/// evenly, the value axis runs from 0 to the highest p95.
struct Geometry {
    top: f64,
    count: f64,
}

impl Geometry {
    /// `None` when no bucket has data.
    fn of(buckets: &[Bucket]) -> Option<Self> {
        let top = buckets
            .iter()
            .filter_map(|b| b.stats.map(|s| s.p95.max(s.p50)))
            .max()?
            .max(1);
        #[allow(clippy::cast_precision_loss)] // a handful of buckets, latencies in ms
        let (top, count) = (top as f64, buckets.len().max(2) as f64);
        Some(Self { top, count })
    }

    fn x(&self, i: usize) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let i = i as f64;
        PAD + i * (WIDTH - 2.0 * PAD) / (self.count - 1.0)
    }

    fn y(&self, ms: i64) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let ms = ms as f64;
        HEIGHT - PAD - (ms / self.top) * (HEIGHT - 2.0 * PAD)
    }
}

/// The chart as an `<svg>` string; `None` when no bucket has data.
///
/// Empty buckets break the lines, so outages show as gaps.
pub(crate) fn latency_svg(buckets: &[Bucket], label: &str) -> Option<String> {
    let geometry = Geometry::of(buckets)?;
    let path = |pick: fn(&uptime_domain::latency::Summary) -> i64| {
        let mut d = String::new();
        let mut pen_down = false;
        for (i, bucket) in buckets.iter().enumerate() {
            match &bucket.stats {
                Some(stats) => {
                    let _ = write!(
                        d,
                        "{}{:.1} {:.1}",
                        if pen_down { " L" } else { "M" },
                        geometry.x(i),
                        geometry.y(pick(stats))
                    );
                    pen_down = true;
                }
                None => pen_down = false,
            }
        }
        d
    };
    Some(format!(
        r#"<svg class="chart" viewBox="0 0 {WIDTH} {HEIGHT}" preserveAspectRatio="none" role="img" aria-label="{label}"><path class="chart-p95" d="{p95}" fill="none" vector-effect="non-scaling-stroke"/><path class="chart-p50" d="{p50}" fill="none" vector-effect="non-scaling-stroke"/></svg>"#,
        label = label.replace('"', "&quot;"),
        p95 = path(|s| s.p95),
        p50 = path(|s| s.p50),
    ))
}

/// What the hover needs, as a JSON array with one entry per bucket:
/// `[start, p50, p95, x %, p50 y %, p95 y %]`. Positions are percentages of the
/// chart box, on the same axes as [`latency_svg`]; empty buckets have `null`
/// figures. `[]` when no bucket has data.
pub(crate) fn latency_points(buckets: &[Bucket]) -> String {
    let Some(geometry) = Geometry::of(buckets) else {
        return "[]".to_owned();
    };
    let percent = |value: f64, of: f64| (value / of * 10_000.0).round() / 100.0;
    let points: Vec<serde_json::Value> = buckets
        .iter()
        .enumerate()
        .map(|(i, bucket)| {
            let x = percent(geometry.x(i), WIDTH);
            match &bucket.stats {
                Some(stats) => serde_json::json!([
                    bucket.start.to_string(),
                    stats.p50,
                    stats.p95,
                    x,
                    percent(geometry.y(stats.p50), HEIGHT),
                    percent(geometry.y(stats.p95), HEIGHT),
                ]),
                None => serde_json::json!([bucket.start.to_string(), null, null, x, null, null]),
            }
        })
        .collect();
    serde_json::Value::Array(points).to_string()
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use uptime_domain::latency::Summary;

    use super::*;

    fn bucket(stats: Option<(i64, i64)>) -> Bucket {
        Bucket {
            start: Timestamp::UNIX_EPOCH,
            stats: stats.map(|(p50, p95)| Summary {
                count: 1,
                p50,
                p95,
                max: p95,
            }),
        }
    }

    #[test]
    fn no_data_draws_nothing() {
        assert_eq!(latency_svg(&[bucket(None), bucket(None)], "x"), None);
        assert_eq!(latency_svg(&[], "x"), None);
    }

    #[test]
    fn gaps_start_a_new_subpath() {
        let svg = latency_svg(
            &[bucket(Some((10, 20))), bucket(None), bucket(Some((10, 20)))],
            "Response time",
        )
        .unwrap();
        assert!(svg.contains(r#"aria-label="Response time""#));
        let p50 = svg.split(r#"class="chart-p50" d=""#).nth(1).unwrap();
        let d = p50.split('"').next().unwrap();
        assert_eq!(d.matches('M').count(), 2, "{d}");
        assert!(!d.contains('L'));
    }

    #[test]
    fn the_highest_p95_touches_the_top_edge() {
        let svg = latency_svg(&[bucket(Some((50, 100))), bucket(Some((50, 100)))], "x").unwrap();
        assert!(svg.contains("M4.0 4.0"), "{svg}");
    }

    #[test]
    fn points_describe_every_bucket_for_the_hover() {
        let mut first = bucket(Some((50, 100)));
        first.start = "2026-09-29T12:00:00Z".parse().unwrap();
        let mut second = bucket(None);
        second.start = "2026-09-29T12:15:00Z".parse().unwrap();
        let mut third = bucket(Some((25, 50)));
        third.start = "2026-09-29T12:30:00Z".parse().unwrap();

        let points = latency_points(&[first, second, third]);

        let parsed: serde_json::Value = serde_json::from_str(&points).unwrap();
        // [start, p50, p95, x %, p50 y %, p95 y %]; empty buckets have no figures.
        assert_eq!(
            parsed[0],
            serde_json::json!(["2026-09-29T12:00:00Z", 50, 100, 0.67, 50.0, 3.33])
        );
        assert_eq!(
            parsed[1],
            serde_json::json!(["2026-09-29T12:15:00Z", null, null, 50.0, null, null])
        );
        assert_eq!(parsed[2][3], serde_json::json!(99.33));
    }

    #[test]
    fn points_line_up_with_the_drawn_lines() {
        // The same geometry as the paths: the top value sits at the top pad.
        let points: serde_json::Value = serde_json::from_str(&latency_points(&[
            bucket(Some((50, 100))),
            bucket(Some((50, 100))),
        ]))
        .unwrap();
        let top = 4.0 / 120.0 * 100.0;
        assert_eq!(
            points[0][5].as_f64().unwrap(),
            (top * 100.0_f64).round() / 100.0
        );
    }
}
