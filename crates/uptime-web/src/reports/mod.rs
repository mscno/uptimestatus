//! Monthly SLA reports for status pages: the figures ([`Report::build`]),
//! who may see them ([`Access`]), and the PDF rendition ([`pdf`]), cached
//! by content ([`PdfCache`]).
//!
//! Reports are built from the same history as the status page: component
//! tallies (checks, manual statuses and overrides, with excused checks
//! counting as up), incidents and maintenance windows. Past months only
//! change when history is corrected; the cache key is a hash of the report
//! itself, so corrections show at once without explicit invalidation.

pub(crate) mod pdf;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use topcoat::Result;
use uptime_domain::{
    BarLevel, MonitorId, Month, Tally, bar_level,
    component::{component_bar_level, day_span},
    month_figures, overall_uptime,
};
use uptime_store::{AuditQuery, Page, ReportLink, ReportSettings, Store};

use crate::fmt;

/// How uptime is measured, printed under every report.
const METHODOLOGY: [&str; 4] = [
    "Uptime is the share of checks that were not down, or for a component whose status is set by hand, the share of time. Pending retries and degraded performance count as up; maintenance windows are left out.",
    "A partial outage set by hand counts half its time as down. Failed checks an administrator excused (for example, problems with the monitoring itself) count as up, and each excuse is recorded in the audit trail.",
    "Downtime and maintenance time are the observed time in proportion to the checks (or time) down and in maintenance; for checked components they are estimates at the check interval. Days and months are UTC.",
    "The overall figure is the average of the components' uptime.",
];

/// The furthest back a report list goes.
const MAX_MONTHS: usize = 60;

/// One month of a status page, ready to show (HTML) or typeset (PDF).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) title: String,
    /// `2026-09`.
    pub(crate) month: String,
    /// "September 2026".
    pub(crate) label: String,
    /// The month is not over yet.
    pub(crate) to_date: bool,
    pub(crate) generated: String,
    /// `#rrggbb`.
    pub(crate) accent: String,
    /// `pixel` or `clean`.
    pub(crate) look: String,
    /// The logo's path for the PDF (`/logo.png`); set when rendering.
    pub(crate) logo: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) overall: Option<String>,
    pub(crate) overall_met: Option<bool>,
    pub(crate) sections: Vec<SectionReport>,
    pub(crate) incidents: Vec<IncidentReport>,
    pub(crate) maintenance: Vec<MaintenanceReport>,
    /// Failed checks in the month that an administrator excused.
    pub(crate) excused: u64,
    pub(crate) methodology: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SectionReport {
    pub(crate) name: String,
    pub(crate) components: Vec<ComponentReport>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ComponentReport {
    pub(crate) name: String,
    /// "99.95%".
    pub(crate) uptime: Option<String>,
    pub(crate) downtime: String,
    pub(crate) downtime_secs: u64,
    pub(crate) maintenance: String,
    /// Against the page's target, when it has one.
    pub(crate) met: Option<bool>,
    /// One per day of the month: `up`, `warn`, `down`, `empty` or `future`.
    pub(crate) bars: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct IncidentReport {
    pub(crate) title: String,
    pub(crate) impact: String,
    pub(crate) started: String,
    pub(crate) duration: String,
    pub(crate) components: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MaintenanceReport {
    pub(crate) title: String,
    pub(crate) starts: String,
    pub(crate) ends: String,
}

impl Report {
    /// `page`'s report for `month` as of `now` (`month` must have started).
    pub(crate) async fn build(
        store: &Store,
        page: &Page,
        settings: &ReportSettings,
        month: Month,
        now: Timestamp,
    ) -> Result<Self> {
        let today = now.to_zoned(TimeZone::UTC).date();
        let last = month.last_day().min(today);
        let monitors = store.list_monitors().await?;
        let by_key: HashMap<&str, (MonitorId, &str)> = monitors
            .iter()
            .map(|m| (m.spec.key.as_str(), (m.id, m.spec.name.as_str())))
            .collect();
        let changes = store.component_changes(page.id).await?;
        let mut sections = Vec::new();
        let mut uptimes = Vec::new();
        let mut names_by_monitor: HashMap<MonitorId, String> = HashMap::new();
        for section in &page.spec.sections {
            let mut components = Vec::new();
            for component in &section.components {
                let key = component.key().as_str();
                let monitor = component
                    .monitor_key()
                    .and_then(|k| by_key.get(k.as_str()).copied());
                if component.monitor_key().is_some() && monitor.is_none() {
                    continue;
                }
                let name = component.label().map_or_else(
                    || monitor.map_or_else(|| key.to_owned(), |(_, name)| name.to_owned()),
                    str::to_owned,
                );
                if let Some((id, _)) = monitor {
                    names_by_monitor.entry(id).or_insert_with(|| name.clone());
                }
                let days = store
                    .component_daily_tallies(page.id, key, month.first_day(), last, now)
                    .await?;
                let manual = changes
                    .get(key)
                    .is_some_and(|history| history.iter().any(|c| c.status.is_some()));
                let mut total = Tally::default();
                let mut observed = 0;
                for (day, tally) in &days {
                    total.merge(tally);
                    observed += day_span(*day, now);
                }
                let figures = month_figures(&total, observed);
                uptimes.push(figures.uptime);
                let level = if manual {
                    component_bar_level
                } else {
                    bar_level
                };
                components.push(ComponentReport {
                    name,
                    uptime: figures.uptime.map(fmt::uptime),
                    downtime: duration(figures.downtime_secs),
                    downtime_secs: figures.downtime_secs,
                    maintenance: duration(figures.maintenance_secs),
                    met: settings
                        .sla_target
                        .zip(figures.uptime)
                        .map(|(target, uptime)| target.met(uptime)),
                    bars: month
                        .days()
                        .into_iter()
                        .map(|day| {
                            if day > today {
                                "future"
                            } else {
                                bar_class(level(days.get(&day)))
                            }
                        })
                        .collect(),
                });
            }
            sections.push(SectionReport {
                name: section.name.clone(),
                components,
            });
        }
        let overall = overall_uptime(uptimes);
        let ids: Vec<MonitorId> = names_by_monitor.keys().copied().collect();
        let incidents = store
            .incidents_overlapping(&ids, month.start(), month.end())
            .await?
            .into_iter()
            .rev()
            .map(|incident| IncidentReport {
                started: fmt::when(incident.started_at),
                duration: incident.resolved_at.map_or_else(
                    || "ongoing".to_owned(),
                    |resolved| {
                        duration(
                            u64::try_from(resolved.duration_since(incident.started_at).as_secs())
                                .unwrap_or(0),
                        )
                    },
                ),
                components: incident
                    .monitor_ids
                    .iter()
                    .filter_map(|id| names_by_monitor.get(id).cloned())
                    .collect(),
                impact: incident.impact.label().to_owned(),
                title: incident.title,
            })
            .collect();
        let on_page: HashSet<MonitorId> = ids.iter().copied().collect();
        let maintenance = store
            .maintenances_between(month.start(), month.end())
            .await?
            .into_iter()
            .filter(|m| m.monitor_ids.iter().any(|id| on_page.contains(id)))
            .map(|m| MaintenanceReport {
                title: m.spec.title,
                starts: fmt::when(m.spec.starts_at),
                ends: fmt::when(m.spec.ends_at),
            })
            .collect();
        let excused = excused_checks(store, &ids, month).await?;
        Ok(Self {
            title: page.spec.title.clone(),
            month: month.to_string(),
            label: month.label(),
            to_date: month.contains(now),
            generated: fmt::when_dated(now),
            accent: page
                .spec
                .accent
                .as_ref()
                .map_or_else(|| "#7c5cff".to_owned(), |a| a.as_str().to_owned()),
            look: page.spec.look.as_str().to_owned(),
            logo: None,
            target: settings.sla_target.map(|t| t.to_string()),
            overall: overall.map(fmt::uptime),
            overall_met: settings.sla_target.zip(overall).map(|(t, u)| t.met(u)),
            sections,
            incidents,
            maintenance,
            excused,
            methodology: METHODOLOGY.iter().map(|line| (*line).to_owned()).collect(),
        })
    }

    /// Identifies the report's content (and the logo it shows): equal for
    /// equal reports, different after any correction.
    pub(crate) fn fingerprint(&self, logo: Option<&[u8]>) -> String {
        let mut hasher = Sha256::new();
        let mut stable = self.clone();
        // The generation time changes every render; the rest is the content.
        stable.generated = String::new();
        hasher.update(serde_json::to_vec(&stable).unwrap_or_default());
        if let Some(logo) = logo {
            hasher.update(logo);
        }
        hex(&hasher.finalize())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn bar_class(level: BarLevel) -> &'static str {
    match level {
        BarLevel::Up => "up",
        BarLevel::Warn => "warn",
        BarLevel::Down => "down",
        BarLevel::None => "empty",
    }
}

fn duration(secs: u64) -> String {
    fmt::duration(std::time::Duration::from_secs(secs))
}

/// Failed checks of `monitors` an administrator excused within `month`: the
/// raw results while they are retained, and otherwise what the audit trail
/// recorded (excused minus counted again), whichever is larger.
async fn excused_checks(store: &Store, monitors: &[MonitorId], month: Month) -> Result<u64> {
    let raw = store
        .excused_checks_between(monitors, month.start(), month.end())
        .await?;
    let events = store
        .audit_events(&AuditQuery {
            action: Some("uptime".into()),
            limit: 1000,
            ..AuditQuery::default()
        })
        .await?;
    let ids: HashSet<i64> = monitors.iter().map(|m| m.0).collect();
    let mut audited: i64 = 0;
    for event in events {
        let details = &event.details;
        let in_month = details["from"]
            .as_str()
            .and_then(|from| from.parse::<Timestamp>().ok())
            .is_some_and(|from| month.start() <= from && from < month.end());
        let touches = match &event.target {
            Some(target) if target.kind == "monitor" => {
                target.id.parse::<i64>().is_ok_and(|id| ids.contains(&id))
            }
            _ => details["monitors"].as_array().is_some_and(|list| {
                list.iter()
                    .filter_map(Value::as_i64)
                    .any(|id| ids.contains(&id))
            }),
        };
        if !(in_month && touches) {
            continue;
        }
        let checks = details["checks"].as_i64().unwrap_or(0);
        match event.action.as_str() {
            "uptime.excuse" => audited += checks,
            "uptime.restore" => audited -= checks,
            _ => {}
        }
    }
    Ok(raw.max(u64::try_from(audited).unwrap_or(0)))
}

/// The months a page has history for, newest first (at most [`MAX_MONTHS`]).
pub(crate) async fn months(store: &Store, page: &Page, now: Timestamp) -> Result<Vec<Month>> {
    let monitors = store.list_monitors().await?;
    let keys: Vec<_> = page.spec.monitor_keys().collect();
    let ids: Vec<MonitorId> = monitors
        .iter()
        .filter(|m| keys.contains(&&m.spec.key))
        .map(|m| m.id)
        .collect();
    let Some(first) = store.first_activity(page.id, &ids).await? else {
        return Ok(Vec::new());
    };
    let first = Month::of(utc_midnight(first));
    let mut month = Month::of(now);
    let mut months = Vec::new();
    while month >= first && months.len() < MAX_MONTHS {
        months.push(month);
        month = month.previous();
    }
    Ok(months)
}

fn utc_midnight(day: Date) -> Timestamp {
    day.to_zoned(TimeZone::UTC)
        .map_or(Timestamp::UNIX_EPOCH, |zoned| zoned.timestamp())
}

/// Who may open a page's reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Anyone: reports are public with a published page.
    Public,
    /// A signed-in admin.
    Admin,
    /// A report link, perhaps for one month only.
    Link(ReportLink),
}

impl Access {
    pub(crate) fn opens(&self, month: Month, now: Timestamp) -> bool {
        match self {
            Self::Public | Self::Admin => true,
            Self::Link(link) => link.opens(month, now),
        }
    }

    /// Whether the whole list of months is visible.
    pub(crate) fn lists_months(&self, now: Timestamp) -> bool {
        match self {
            Self::Public | Self::Admin => true,
            Self::Link(link) => link.month.is_none() && link.expires_at.is_none_or(|at| now < at),
        }
    }
}

/// A fresh report link key: `rpt_` and 24 random bytes, base64url.
pub(crate) fn new_link_key() -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 24];
    if let Err(error) = getrandom::fill(&mut bytes) {
        panic!("the OS random number generator is unavailable: {error}");
    }
    format!("rpt_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// How a link key is stored: SHA-256, hex.
pub(crate) fn hash_link_key(key: &str) -> String {
    hex(&Sha256::digest(key.trim().as_bytes()))
}

/// A rendered PDF and its report's fingerprint.
type CachedPdf = (String, Arc<Vec<u8>>);

/// Rendered PDFs by report fingerprint (a handful; past months are asked for
/// again and again, current ones change).
#[derive(Clone, Debug, Default)]
pub struct PdfCache {
    /// Least recently used first.
    entries: Arc<Mutex<Vec<CachedPdf>>>,
}

/// How many PDFs are kept.
const PDF_CACHE_SIZE: usize = 12;

impl PdfCache {
    pub(crate) fn get(&self, fingerprint: &str) -> Option<Arc<Vec<u8>>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let index = entries.iter().position(|(key, _)| key == fingerprint)?;
        // Most recently used last.
        let entry = entries.remove(index);
        let pdf = entry.1.clone();
        entries.push(entry);
        Some(pdf)
    }

    pub(crate) fn put(&self, fingerprint: String, pdf: Arc<Vec<u8>>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(key, _)| *key != fingerprint);
        if entries.len() >= PDF_CACHE_SIZE {
            entries.remove(0);
        }
        entries.push((fingerprint, pdf));
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn link_keys_are_random_and_hash_stably() {
        let (a, b) = (new_link_key(), new_link_key());
        assert!(a.starts_with("rpt_") && a.len() > 30);
        assert_ne!(a, b);
        assert_eq!(hash_link_key(&a), hash_link_key(&format!(" {a} ")));
        assert_eq!(hash_link_key(&a).len(), 64);
    }

    #[test]
    fn the_pdf_cache_keeps_the_most_recently_used() {
        let cache = PdfCache::default();
        for n in 0..PDF_CACHE_SIZE {
            cache.put(n.to_string(), Arc::new(vec![n as u8]));
        }
        assert!(cache.get("0").is_some(), "touch 0");
        cache.put("new".into(), Arc::new(Vec::new()));
        assert!(cache.get("0").is_some(), "kept: recently used");
        assert!(cache.get("1").is_none(), "evicted: least recently used");
        assert!(cache.get("new").is_some());
    }

    #[test]
    fn month_links_open_only_their_month_until_they_expire() {
        let september: Month = "2026-09".parse().unwrap();
        let link = ReportLink {
            id: 1,
            page_id: 1,
            month: Some(september),
            label: None,
            expires_at: Some("2026-11-01T00:00:00Z".parse().unwrap()),
            created_by: "alice".into(),
            created_at: Timestamp::UNIX_EPOCH,
        };
        let now: Timestamp = "2026-10-15T00:00:00Z".parse().unwrap();
        let access = Access::Link(link);
        assert!(access.opens(september, now));
        assert!(!access.opens(september.next(), now));
        assert!(!access.lists_months(now));
        assert!(!access.opens(september, "2026-11-02T00:00:00Z".parse().unwrap()));
        assert!(Access::Public.lists_months(now));
    }
}
