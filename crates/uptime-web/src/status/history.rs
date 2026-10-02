//! The Maintenance and Previous incidents tabs, three months at a time, and
//! one incident's whole timeline.
//!
//! Months and days are UTC (like the uptime bars); exact times are spelled in
//! the visitor's time zone.

use std::collections::HashSet;

use jiff::{Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use topcoat::{
    Result,
    context::Cx,
    router::{error::not_found, header, page, path_param, query_params},
    view::{View, component, view},
};
use uptime_domain::MonitorId;
use uptime_store::{Incident, Maintenance};

use super::{
    Slug, StatusView, Tab, base, cache_control, frame, home, incident_pill, load, services,
};
use crate::{auth::app, views::local_time};

/// Three consecutive months: what a history tab shows, newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Months {
    /// The first day of the earliest month.
    first: Date,
}

impl Months {
    /// The three months starting `offset` months from `month`'s.
    fn starting(month: Date, offset: i64) -> Option<Self> {
        let first = month.first_of_month().checked_add(offset.months()).ok()?;
        // Both neighbours must exist, for paging.
        first.checked_sub(3.months()).ok()?;
        first.checked_add(6.months()).ok()?;
        Some(Self { first })
    }

    /// From `?from=2026-08`.
    fn parse(from: &str) -> Option<Self> {
        let (year, month) = from.split_once('-')?;
        if year.len() != 4 || month.len() != 2 {
            return None;
        }
        Self::starting(
            Date::new(year.parse().ok()?, month.parse().ok()?, 1).ok()?,
            0,
        )
    }

    /// Oldest first.
    fn each(self) -> Vec<Date> {
        (0..3)
            .filter_map(|n| self.first.checked_add(n.months()).ok())
            .collect()
    }

    fn last(self) -> Date {
        self.first.saturating_add(2.months())
    }

    fn start(self) -> Result<Timestamp> {
        Ok(self.first.to_zoned(TimeZone::UTC)?.timestamp())
    }

    fn end(self) -> Result<Timestamp> {
        Ok(self
            .first
            .checked_add(3.months())?
            .to_zoned(TimeZone::UTC)?
            .timestamp())
    }

    /// "Sep 2026 to Nov 2026".
    fn label(self) -> String {
        format!(
            "{} to {}",
            self.first.strftime("%b %Y"),
            self.last().strftime("%b %Y")
        )
    }

    /// `?from=` for the three months before (`-1`) or after (`1`).
    fn page(self, direction: i64) -> String {
        let first = self.first.saturating_add((3 * direction).months());
        format!("?from={}", first.strftime("%Y-%m"))
    }
}

/// The first day of `at`'s month (UTC).
fn month_of(at: Timestamp) -> Date {
    at.to_zoned(TimeZone::UTC).date().first_of_month()
}

#[query_params]
pub(crate) struct RangeQuery {
    from: Option<String>,
}

/// The months asked for, else `offset` months from this one onwards.
fn months(cx: &Cx, this_month: Date, offset: i64) -> Result<Months> {
    let asked = query_params::<RangeQuery>(cx)
        .ok()
        .and_then(|query| query.from.as_deref().and_then(Months::parse));
    Ok(asked
        .or_else(|| Months::starting(this_month, offset))
        .ok_or_else(not_found)?)
}

/// The range and the arrows to the neighbouring ranges.
#[component]
async fn pager(months: Months, later: bool) -> Result<impl View> {
    Ok(view! {
        <nav class="pager" aria-label="Months">
            <span class="pager-range">(months.label())</span>
            <a class="pager-btn" href=(months.page(-1)) aria-label="Earlier months" title="Earlier months">"‹"</a>
            if later {
                <a class="pager-btn" href=(months.page(1)) aria-label="Later months" title="Later months">"›"</a>
            } else {
                <span class="pager-btn" aria-disabled="true">"›"</span>
            }
        </nav>
    })
}

/// Affected services as chips.
#[component]
async fn service_chips(names: &[String]) -> Result<impl View> {
    Ok(view! {
        if !names.is_empty() {
            <ul class="chips" aria-label="Affected services">
                for name in names {
                    <li>(name.as_str())</li>
                }
            </ul>
        }
    })
}

// ── Maintenance ──────────────────────────────────────────────────────────

/// The page's windows starting within `months`, newest first.
async fn page_windows(cx: &Cx, view: &StatusView, months: Months) -> Result<Vec<Maintenance>> {
    let on_page: HashSet<MonitorId> = view.monitor_ids.iter().copied().collect();
    let start = months.start()?;
    let mut windows: Vec<Maintenance> = app(cx)
        .store
        .maintenances_between(start, months.end()?)
        .await?
        .into_iter()
        .filter(|m| m.spec.starts_at >= start)
        .filter(|m| m.monitor_ids.iter().any(|id| on_page.contains(id)))
        .collect();
    windows.sort_by_key(|m| std::cmp::Reverse(m.spec.starts_at));
    Ok(windows)
}

#[component]
async fn maintenance_item(
    view: &StatusView,
    window: &Maintenance,
    now: Timestamp,
) -> Result<impl View> {
    let spec = &window.spec;
    let (pill, label) = if spec.ends_at <= now {
        ("pill pill-up", "Completed")
    } else if spec.is_active(now) {
        ("pill pill-maintenance", "In progress")
    } else {
        ("pill pill-maintenance", "Scheduled")
    };
    let names = services(view, &window.monitor_ids);
    Ok(view! {
        <article class="history-item">
            <header>
                <strong class="history-title">(spec.title.as_str())</strong>
                <span class=(pill)>(label)</span>
            </header>
            <p class="small muted">local_time(at: spec.starts_at) " → " local_time(at: spec.ends_at)</p>
            if let Some(description) = &spec.description {
                <p>(description.as_str())</p>
            }
            service_chips(names: &names)
        </article>
    })
}

#[page("/s/{slug}/maintenance")]
pub(crate) async fn maintenance_page(cx: &Cx) -> Result<impl View> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let base = base(cx, &view.slug);
    let now = Timestamp::now();
    let this_month = month_of(now);
    // Last month, this month and next.
    let months = months(cx, this_month, -1)?;
    let windows = page_windows(cx, &view, months).await?;
    let cards: Vec<(Date, Vec<Maintenance>)> = months
        .each()
        .into_iter()
        .rev()
        .map(|month| {
            let inside = windows
                .iter()
                .filter(|w| month_of(w.spec.starts_at) == month)
                .cloned()
                .collect();
            (month, inside)
        })
        .collect();
    let title = format!("Maintenance · {}", view.title);
    Ok(view! {
        ((header::CACHE_CONTROL, cache_control(&view)))
        frame(view: &view, base: &base, tab: Tab::Maintenance, title: &title,
            <div class="history">
                pager(months: months, later: true)
                for (month, windows) in &cards {
                    <section class="month-card">
                        <h3>(month.strftime("%B %Y").to_string())</h3>
                        if windows.is_empty() {
                            <p class="empty">(if *month >= this_month { "No maintenance scheduled" } else { "No maintenance" })</p>
                        }
                        for window in windows {
                            maintenance_item(view: &view, window: window, now: now)
                        }
                    </section>
                }
            </div>
        )
    })
}

// ── Incidents ────────────────────────────────────────────────────────────

/// "1 incident", "2 incidents".
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[component]
async fn incident_item(incident: &Incident, base: &str) -> Result<impl View> {
    let href = format!("{base}/incidents/{}", incident.id);
    let earlier = incident.updates.len().saturating_sub(1);
    Ok(view! {
        <article class="history-item">
            <header>
                <a class="history-title" href=(href.as_str())>(incident.title.as_str())</a>
                <span class=(incident_pill(incident.status, incident.impact))>(incident.status.label())</span>
            </header>
            if let Some(latest) = incident.updates.first() {
                <ol class="timeline">
                    <li>
                        <span class="small muted">(latest.status.label()) " · " local_time(at: latest.created_at)</span>
                        <p>(latest.body.as_str())</p>
                    </li>
                </ol>
            }
            if earlier > 0 {
                <a class="small more" href=(href.as_str())>(count(earlier, "previous update"))</a>
            }
        </article>
    })
}

/// A month's incidents by day, in the order given (newest first).
type MonthIncidents = Vec<(Date, Vec<Incident>)>;

fn by_day(incidents: &[Incident], month: Date) -> MonthIncidents {
    let mut days = MonthIncidents::new();
    for incident in incidents.iter().cloned() {
        let day = incident.started_at.to_zoned(TimeZone::UTC).date();
        if day.first_of_month() != month {
            continue;
        }
        match days.last_mut() {
            Some((last, list)) if *last == day => list.push(incident),
            _ => days.push((day, vec![incident])),
        }
    }
    days
}

#[page("/s/{slug}/incidents")]
pub(crate) async fn incidents_page(cx: &Cx) -> Result<impl View> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let base = base(cx, &view.slug);
    let this_month = month_of(Timestamp::now());
    // The last three months, up to this one.
    let months = months(cx, this_month, -2)?;
    // Newest first, so days and incidents come out newest first too.
    let incidents = app(cx)
        .store
        .incident_history(&view.monitor_ids, months.start()?, months.end()?)
        .await?;
    let cards: Vec<(Date, MonthIncidents)> = months
        .each()
        .into_iter()
        .rev()
        .map(|month| (month, by_day(&incidents, month)))
        .collect();
    let title = format!("Previous incidents · {}", view.title);
    Ok(view! {
        ((header::CACHE_CONTROL, cache_control(&view)))
        frame(view: &view, base: &base, tab: Tab::Incidents, title: &title,
            <div class="history">
                pager(months: months, later: months.last() < this_month)
                for (month, days) in &cards {
                    <section class="month-card">
                        <h3>(month.strftime("%B %Y").to_string())</h3>
                        if days.is_empty() {
                            <p class="empty">"No incidents reported"</p>
                        }
                        for (day, incidents) in days {
                            <h4 class="history-day">(day.strftime("%b %d, %Y").to_string()) " · " (count(incidents.len(), "incident"))</h4>
                            for incident in incidents {
                                incident_item(incident: incident, base: &base)
                            }
                        }
                    </section>
                }
            </div>
        )
    })
}

path_param!(incident_id: i64, error = not_found);

/// One incident: what it touched and every update.
#[page("/s/{slug}/incidents/{incident_id}")]
pub(crate) async fn incident_page(cx: &Cx) -> Result<impl View> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let id = *path_param::<IncidentId>(cx)?;
    let incident = app(cx)
        .store
        .public_incident(id)
        .await?
        .ok_or_else(not_found)?;
    let on_page = incident
        .monitor_ids
        .iter()
        .any(|id| view.monitor_ids.contains(id));
    // Only incidents affecting this page are visible here.
    if !on_page {
        return Err(not_found().into());
    }
    let base = base(cx, &view.slug);
    let names = services(&view, &incident.monitor_ids);
    let title = format!("{} · {}", incident.title, view.title);
    Ok(view! {
        ((header::CACHE_CONTROL, cache_control(&view)))
        frame(view: &view, base: &base, tab: Tab::Incidents, title: &title,
            <div class="history">
                <a class="back-link" href=(home(&base))>"← Back to overview"</a>
                <article class="incident-head">
                    <span class=(incident_pill(incident.status, incident.impact))>(incident.status.label())</span>
                    <h2>(incident.title.as_str())</h2>
                    <p class="small muted">
                        "Started " local_time(at: incident.started_at)
                        if let Some(resolved) = incident.resolved_at {
                            " · resolved " local_time(at: resolved)
                        }
                    </p>
                    if !names.is_empty() {
                        <h3 class="label">"Affected services"</h3>
                        service_chips(names: &names)
                    }
                </article>
                <ol class="updates">
                    for update in &incident.updates {
                        <li class="update">
                            <header>
                                <strong>(update.status.label())</strong>
                                <span class="small muted">local_time(at: update.created_at)</span>
                            </header>
                            <p>(update.body.as_str())</p>
                        </li>
                    }
                </ol>
            </div>
        )
    })
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn months_page_by_three() {
        let months = Months::starting(date(2026, 9, 29), -1).unwrap();
        assert_eq!(
            months.each(),
            [date(2026, 8, 1), date(2026, 9, 1), date(2026, 10, 1)]
        );
        assert_eq!(months.label(), "Aug 2026 to Oct 2026");
        assert_eq!(months.page(-1), "?from=2026-05");
        assert_eq!(months.page(1), "?from=2026-11");
        assert_eq!(
            months.start().unwrap(),
            "2026-08-01T00:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            months.end().unwrap(),
            "2026-11-01T00:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    #[rstest]
    #[case("2026-08", Some(date(2026, 8, 1)))]
    #[case("2026-8", None)]
    #[case("2026-13", None)]
    #[case("soon", None)]
    #[case("9999-12", None)]
    #[case("-001-01", None)]
    fn ranges_parse_from_year_and_month(#[case] from: &str, #[case] first: Option<Date>) {
        assert_eq!(Months::parse(from).map(|m| m.first), first);
    }
}
