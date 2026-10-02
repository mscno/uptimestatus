//! Uptime windows and latency charts for a monitor, on the console and the
//! public status pages.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use topcoat::{
    Result,
    view::{Unescaped, View, component, view},
};
use uptime_domain::{
    MonitorId, Tally, UptimeWindows,
    latency::{self, Sample, Summary},
};
use uptime_store::{Store, WindowCheck};

use crate::{chart, fmt};

/// How much history a latency chart covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Range {
    #[default]
    Day,
    Week,
}

impl Range {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }

    fn span(self) -> SignedDuration {
        SignedDuration::from_hours(match self {
            Self::Day => 24,
            Self::Week => 7 * 24,
        })
    }

    /// Slices on the time axis.
    fn buckets(self) -> usize {
        match self {
            Self::Day => 96,
            Self::Week => 84,
        }
    }
}

/// Everything a metrics panel shows.
#[derive(Clone, Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) windows: UptimeWindows,
    pub(crate) summary: Option<Summary>,
    /// The chart, when any check in the range measured a response time.
    pub(crate) svg: Option<String>,
    /// The hover's per-bucket figures (see [`chart::latency_points`]).
    pub(crate) points: String,
}

impl Metrics {
    /// Builds the panel from checks since `now - range` and the daily tallies
    /// of the last 90 days.
    pub(crate) fn build(
        now: Timestamp,
        range: Range,
        checks: &[WindowCheck],
        dailies: &BTreeMap<Date, Tally>,
    ) -> Self {
        let recent: Vec<_> = checks
            .iter()
            .map(|c| (c.checked_at, c.state_after))
            .collect();
        let from = now.checked_sub(range.span()).unwrap_or(now);
        let samples: Vec<Sample> = checks
            .iter()
            .filter_map(|c| {
                Some(Sample {
                    at: c.checked_at,
                    latency_ms: c.latency_ms?,
                })
            })
            .filter(|s| s.at >= from)
            .collect();
        let buckets = latency::buckets(&samples, from, now, range.buckets());
        Self {
            windows: UptimeWindows::compute(now, &recent, dailies),
            summary: latency::summarize(&samples),
            svg: chart::latency_svg(&buckets, "Response time, median and 95th percentile"),
            points: chart::latency_points(&buckets),
        }
    }

    /// Loads what [`Metrics::build`] needs for `monitor`.
    pub(crate) async fn load(
        store: &Store,
        monitor: MonitorId,
        range: Range,
        now: Timestamp,
    ) -> Result<Self> {
        // Windowed uptime always needs the last 24 hours, whatever the range.
        let since = now.checked_sub(range.span().max(SignedDuration::from_hours(24)))?;
        let checks = store.checks_since(monitor, since).await?;
        let today = now.to_zoned(TimeZone::UTC).date();
        let dailies = store
            .daily_tallies(&[monitor], today.checked_sub(89.days())?, today)
            .await?
            .remove(&monitor)
            .unwrap_or_default();
        Ok(Self::build(now, range, &checks, &dailies))
    }
}

fn percent(ratio: Option<f64>) -> String {
    ratio.map_or("—".into(), fmt::uptime)
}

/// Uptime for the last 24 hours, 7, 30 and 90 days.
#[component]
pub(crate) async fn uptime_windows(windows: &UptimeWindows) -> Result<impl View> {
    let cells = [
        ("24 hours", windows.h24),
        ("7 days", windows.d7),
        ("30 days", windows.d30),
        ("90 days", windows.d90),
    ];
    Ok(view! {
        <dl class="windows">
            for (label, value) in &cells {
                <div><dt>(*label)</dt><dd>(percent(*value))</dd></div>
            }
        </dl>
    })
}

/// Moves the chart's guide, markers and card to the bucket under the pointer.
/// Runs on the chart box (`el`), which carries the buckets in `data-points`
/// (`[start, p50, p95, x %, p50 y %, p95 y %]`); it sets `--x`, `--y50` and
/// `--y95` on the box and fills the card. Nothing is stored anywhere else, so
/// a live re-render only drops the hover until the pointer moves again.
const CHART_MOVE: &str = "\
let p = (el._pts ||= JSON.parse(el.dataset.points)); \
if (!p.length) return; \
let r = el.getBoundingClientRect(), f = Math.min(Math.max((evt.clientX - r.left) / r.width, 0), 1) * 100, i = 0; \
p.forEach((q, n) => { if (Math.abs(q[3] - f) < Math.abs(p[i][3] - f)) i = n }); \
let [t, a, b, x, ya, yb] = p[i], set = (s, v) => el.querySelector(s).textContent = v; \
el.style.setProperty('--x', x + '%'); \
if (a === null) { el.dataset.empty = ''; } else { delete el.dataset.empty; el.style.setProperty('--y50', ya + '%'); el.style.setProperty('--y95', yb + '%'); } \
el.dataset.side = x > 55 ? 'left' : 'right'; \
set('.chart-tip-time', new Date(t).toLocaleString(undefined, {month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit'})); \
set('.chart-tip-p50', a === null ? 'no data' : a + ' ms'); \
set('.chart-tip-p95', b === null ? '' : b + ' ms'); \
el.dataset.hover = ''";

/// The response-time chart with its percentile figures. Hovering (or dragging
/// a finger across) it shows the median and p95 of the bucket under the pointer.
#[component]
pub(crate) async fn latency_chart(metrics: &Metrics) -> Result<impl View> {
    Ok(view! {
        if let Some(svg) = &metrics.svg {
            <figure class="latency">
                <div class="chart-box" data-points=(metrics.points.as_str())
                    data-on:pointermove=(CHART_MOVE)
                    data-on:pointerleave="delete el.dataset.hover">
                    (Unescaped::new_unchecked(svg.clone()))
                    <span class="chart-guide" aria-hidden="true"></span>
                    <span class="chart-dot chart-dot-p95" aria-hidden="true"></span>
                    <span class="chart-dot chart-dot-p50" aria-hidden="true"></span>
                    <div class="chart-tip" role="tooltip" aria-hidden="true">
                        <div class="chart-tip-time"></div>
                        <div class="chart-tip-row"><span>"median"</span><strong class="chart-tip-p50"></strong></div>
                        <div class="chart-tip-row"><span>"p95"</span><strong class="chart-tip-p95"></strong></div>
                    </div>
                </div>
                if let Some(summary) = &metrics.summary {
                    <figcaption class="small muted">
                        "median " (format!("{}ms", summary.p50))
                        " · p95 " (format!("{}ms", summary.p95))
                        " · max " (format!("{}ms", summary.max))
                    </figcaption>
                }
            </figure>
        } else {
            <p class="muted small">"No response times in this range."</p>
        }
    })
}

#[cfg(test)]
mod tests {
    use uptime_domain::MonitorState;

    use super::*;

    #[test]
    fn range_round_trips() {
        for range in [Range::Day, Range::Week] {
            assert_eq!(Range::parse(range.as_str()), Some(range));
        }
        assert_eq!(Range::parse("1y"), None);
    }

    #[test]
    fn metrics_summarise_the_range_only() {
        let now = "2026-09-29T12:00:00Z".parse::<Timestamp>().unwrap();
        let ago = |h: i64| now.checked_sub(SignedDuration::from_hours(h)).unwrap();
        let check = |at, ms| WindowCheck {
            checked_at: at,
            state_after: MonitorState::Up,
            latency_ms: Some(ms),
        };
        let checks = [check(ago(30), 900), check(ago(3), 100), check(ago(2), 300)];

        let metrics = Metrics::build(now, Range::Day, &checks, &BTreeMap::new());

        let summary = metrics.summary.unwrap();
        assert_eq!((summary.count, summary.max), (2, 300));
        assert!(metrics.svg.is_some());
        assert_eq!(metrics.windows.h24, Some(1.0));
    }
}
