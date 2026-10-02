//! Public status pages: `/s/{slug}` (and custom domains, rewritten there).
//!
//! Pages show only display names, states and uptime — never targets, error
//! messages or response bodies. Tabs: the live status (here), maintenance and
//! incident history ([`history`]).

mod feed;
mod history;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use jiff::{Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::Serialize;
use topcoat::{
    Result,
    context::Cx,
    datastar::PatchElements,
    router::{
        HeaderValue,
        content::{
            Json,
            sse::{Event, KeepAlive, Sse},
        },
        error::not_found,
        header, page, path_param,
        request::extensions,
        route,
    },
    view::{Child, View, ViewExt as _, component, view},
};
use uptime_domain::{
    BarLevel, Impact, IncidentStatus, Look, MonitorState, PageStatus, Tally, Theme, bar_level,
    page_status,
};
use uptime_store::Store;

use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

pub(crate) use feed::status_feed;
pub(crate) use history::{incident_page, incidents_page, maintenance_page};

use crate::{
    auth::{app, current_admin},
    fmt,
    host::CustomDomain,
    metrics::{Metrics, Range, latency_chart, uptime_windows},
    views::{ago, document, state_pill},
};

/// Days of history shown as bars.
const DAYS: i64 = 90;
/// How long a built page is reused (outage traffic spikes cost almost nothing).
const CACHE_TTL: Duration = Duration::from_secs(10);

/// Everything a status page renders, built from the database.
#[derive(Clone, Debug)]
pub(crate) struct StatusView {
    slug: String,
    title: String,
    description: Option<String>,
    accent: Option<String>,
    theme: Theme,
    look: Look,
    published: bool,
    status: PageStatus,
    updated_at: Timestamp,
    sections: Vec<SectionView>,
    maintenances: Vec<MaintenanceView>,
    incidents: Vec<IncidentView>,
    /// Uploaded images, as URLs.
    logo: Option<String>,
    favicon: Option<String>,
    /// The product link: URL and text.
    website: Option<(String, String)>,
    /// Monitors on the page: their checks trigger a live refresh.
    monitor_ids: Vec<uptime_domain::MonitorId>,
}

#[derive(Clone, Debug)]
struct MaintenanceView {
    title: String,
    description: Option<String>,
    starts_at: Timestamp,
    ends_at: Timestamp,
    active: bool,
}

#[derive(Clone, Debug)]
struct IncidentView {
    id: i64,
    title: String,
    impact: Impact,
    status: IncidentStatus,
    started_at: Timestamp,
    resolved_at: Option<Timestamp>,
    /// Newest first.
    updates: Vec<(IncidentStatus, String, Timestamp)>,
}

/// How far ahead maintenance is announced, and how long resolved incidents stay.
const MAINTENANCE_AHEAD: jiff::SignedDuration = jiff::SignedDuration::from_hours(7 * 24);
const INCIDENT_HISTORY: jiff::SignedDuration = jiff::SignedDuration::from_hours(14 * 24);

#[derive(Clone, Debug)]
struct SectionView {
    name: String,
    components: Vec<ComponentView>,
}

#[derive(Clone, Debug)]
struct ComponentView {
    monitor_id: uptime_domain::MonitorId,
    name: String,
    state: MonitorState,
    uptime: Option<f64>,
    bars: Vec<(Date, BarLevel, Option<f64>)>,
    metrics: Metrics,
}

/// A built page and when it was built.
type CacheEntry = (Instant, Arc<StatusView>);

/// Built status pages by slug, reused for [`CACHE_TTL`].
#[derive(Clone, Debug, Default)]
pub struct PageCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
}

impl PageCache {
    fn get(&self, slug: &str) -> Option<Arc<StatusView>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(slug)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, view)| view.clone())
    }

    fn put(&self, view: Arc<StatusView>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.insert(view.slug.clone(), (Instant::now(), view));
    }

    /// Forgets every built page (after a page or monitor changes).
    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Loads (or reuses) the page, hiding drafts from everyone but admins.
async fn load(cx: &Cx, slug: &str) -> Result<Arc<StatusView>> {
    let state = app(cx);
    let view = match state.pages.get(slug) {
        Some(view) => view,
        None => {
            let view = Arc::new(build(&state.store, slug).await?.ok_or_else(not_found)?);
            state.pages.put(view.clone());
            view
        }
    };
    if !view.published && current_admin(cx).await?.is_none() {
        return Err(not_found().into());
    }
    Ok(view)
}

async fn build(store: &Store, slug: &str) -> Result<Option<StatusView>> {
    let Some(page) = store.page_by_slug(slug).await? else {
        return Ok(None);
    };
    let overviews = store.monitor_overviews().await?;
    let by_key: HashMap<&str, _> = overviews
        .iter()
        .map(|o| (o.monitor.spec.key.as_str(), o))
        .collect();
    let ids: Vec<_> = page
        .spec
        .monitor_keys()
        .filter_map(|key| by_key.get(key.as_str()))
        .map(|o| o.monitor.id)
        .collect();
    let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
    let first_day = today.checked_sub((DAYS - 1).days())?;
    let tallies = store.daily_tallies(&ids, first_day, today).await?;

    let now = Timestamp::now();
    let since = now.checked_sub(jiff::SignedDuration::from_hours(24))?;
    let mut recent = HashMap::new();
    for id in &ids {
        recent.insert(*id, store.checks_since(*id, since).await?);
    }

    let mut states = Vec::new();
    let sections = page
        .spec
        .sections
        .iter()
        .map(|section| SectionView {
            name: section.name.clone(),
            components: section
                .components
                .iter()
                .filter_map(|component| {
                    let overview = by_key.get(component.monitor.as_str())?;
                    let state = overview.runtime.runtime.state;
                    states.push(state);
                    let days = tallies.get(&overview.monitor.id);
                    let mut total = Tally::default();
                    let bars = (0..DAYS)
                        .filter_map(|offset| first_day.checked_add(offset.days()).ok())
                        .map(|day| {
                            let tally = days.and_then(|days| days.get(&day));
                            if let Some(tally) = tally {
                                total.merge(tally);
                            }
                            (day, bar_level(tally), tally.and_then(Tally::uptime))
                        })
                        .collect();
                    let metrics = Metrics::build(
                        now,
                        Range::Day,
                        recent
                            .get(&overview.monitor.id)
                            .map_or(&[][..], Vec::as_slice),
                        days.unwrap_or(&BTreeMap::new()),
                    );
                    Some(ComponentView {
                        monitor_id: overview.monitor.id,
                        metrics,
                        name: component
                            .label
                            .clone()
                            .unwrap_or_else(|| overview.monitor.spec.name.clone()),
                        state,
                        uptime: total.uptime(),
                        bars,
                    })
                })
                .collect(),
        })
        .collect();

    let page_monitors: std::collections::HashSet<_> = ids.iter().copied().collect();
    let maintenances = store
        .maintenances_between(now, now.checked_add(MAINTENANCE_AHEAD)?)
        .await?
        .into_iter()
        .filter(|m| m.monitor_ids.iter().any(|id| page_monitors.contains(id)))
        .map(|m| MaintenanceView {
            active: m.spec.is_active(now),
            title: m.spec.title,
            description: m.spec.description,
            starts_at: m.spec.starts_at,
            ends_at: m.spec.ends_at,
        })
        .collect();
    let incidents: Vec<IncidentView> = store
        .public_incidents(&ids, now.checked_sub(INCIDENT_HISTORY)?)
        .await?
        .into_iter()
        .map(|i| IncidentView {
            id: i.id,
            title: i.title,
            impact: i.impact,
            status: i.status,
            started_at: i.started_at,
            resolved_at: i.resolved_at,
            updates: i
                .updates
                .into_iter()
                .map(|u| (u.status, u.body, u.created_at))
                .collect(),
        })
        .collect();

    let website = page
        .spec
        .website
        .as_ref()
        .map(|site| (site.url.to_string(), site.link_text()));
    let logo = page.logo.as_deref().map(crate::media::url);
    // The favicon, else the logo: browsers scale either.
    let favicon = page
        .favicon
        .as_deref()
        .map(crate::media::url)
        .or_else(|| logo.clone());
    Ok(Some(StatusView {
        slug: page.spec.slug.to_string(),
        title: page.spec.title,
        description: page.spec.description,
        accent: page.spec.accent.map(|a| a.as_str().to_owned()),
        theme: page.spec.theme,
        look: page.spec.look,
        published: page.spec.published,
        status: incidents
            .iter()
            .filter(|i| i.resolved_at.is_none())
            .fold(page_status(&states), |status, i| {
                status.with_incident(i.impact)
            }),
        updated_at: now,
        sections,
        maintenances,
        incidents,
        logo,
        favicon,
        website,
        monitor_ids: ids,
    }))
}

fn status_color(status: PageStatus) -> &'static str {
    match status {
        PageStatus::Operational => "var(--up)",
        PageStatus::Degraded => "var(--degraded)",
        PageStatus::PartialOutage | PageStatus::MajorOutage => "var(--down)",
        PageStatus::Maintenance => "var(--maintenance)",
        PageStatus::Unknown => "var(--unknown)",
    }
}

fn impact_color(impact: Impact) -> &'static str {
    match impact {
        Impact::None => "var(--sb-info)",
        Impact::Minor => "var(--degraded)",
        Impact::Major | Impact::Critical => "var(--down)",
    }
}

/// A day's bar: its class, its tone for the hover card, and what it says.
fn bar_look(level: BarLevel) -> (&'static str, &'static str, &'static str) {
    match level {
        BarLevel::Up => ("bar-up", "up", "Operational"),
        BarLevel::Warn => ("bar-warn", "warn", "Minor outage"),
        BarLevel::Down => ("bar-down", "down", "Outage"),
        BarLevel::None => ("bar-none", "none", "No data"),
    }
}

/// An incident's status as pill classes: resolved is good news, monitoring
/// is informational, and an ongoing incident is as bad as its impact.
fn incident_pill(status: IncidentStatus, impact: Impact) -> &'static str {
    match (status, impact) {
        (IncidentStatus::Resolved, _) => "pill pill-up",
        (IncidentStatus::Monitoring, _) | (_, Impact::None) => "pill pill-maintenance",
        (_, Impact::Minor) => "pill pill-degraded",
        (_, Impact::Major | Impact::Critical) => "pill pill-down",
    }
}

/// The names of the page's components watching any of `monitors`, in page
/// order.
fn services(view: &StatusView, monitors: &[uptime_domain::MonitorId]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for component in view.sections.iter().flat_map(|s| &s.components) {
        if monitors.contains(&component.monitor_id) && !names.contains(&component.name) {
            names.push(component.name.clone());
        }
    }
    names
}

/// Which tab a status page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Status,
    Maintenance,
    Incidents,
}

/// Where the page's links start: `/s/{slug}`, or nothing on its custom
/// domain (where the page lives at the root).
fn base(cx: &Cx, slug: &str) -> String {
    if extensions(cx).get::<CustomDomain>().is_some() {
        String::new()
    } else {
        format!("/s/{slug}")
    }
}

/// The Status tab's address.
fn home(base: &str) -> &str {
    if base.is_empty() { "/" } else { base }
}

/// Published pages may be cached briefly; drafts never.
fn cache_control(view: &StatusView) -> HeaderValue {
    HeaderValue::from_static(if view.published {
        "public, max-age=10"
    } else {
        "no-store"
    })
}

/// Everything around a tab: the backdrop, header, tabs and footer.
#[component]
async fn frame(
    view: &StatusView,
    base: &str,
    tab: Tab,
    /// The document title, when not the page's own.
    #[default]
    title: &str,
    /// A Datastar `data-init` expression (the live stream).
    #[default]
    init: &str,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    let theme = match view.theme {
        Theme::Auto => "",
        Theme::Light => "light",
        Theme::Dark => "dark",
    };
    let clean = view.look == Look::Clean;
    let title = if title.is_empty() { &view.title } else { title };
    let accent = view.accent.clone().unwrap_or_default();
    let icon = view.favicon.clone().unwrap_or_default();
    let feed = format!("{base}/feed.atom");
    let tabs = [
        (Tab::Status, "Status", home(base).to_owned()),
        (
            Tab::Maintenance,
            "Maintenance",
            format!("{base}/maintenance"),
        ),
        (
            Tab::Incidents,
            "Previous incidents",
            format!("{base}/incidents"),
        ),
    ];
    Ok(view! {
        document(title: title, theme: theme, accent: &accent, icon: &icon,
            look: if clean { "clean" } else { "" },
            modules: if clean { &[] } else { &["starfield"] },
            feed: &feed,
            // Decorative, behind everything; the component pauses offscreen and
            // for visitors who prefer reduced motion. The clean look has none.
            if !clean {
                <sb-starfield class="backdrop" speed="3" density="160" tint="violet" label="" aria-hidden="true"></sb-starfield>
            }
            <main class="status-page" data-init=((!init.is_empty()).then_some(init))>
                <div class="status-shell">
                    if !view.published {
                        <p class="flash">"Draft — only signed-in admins can see this page."</p>
                    }
                    if let Some((url, text)) = &view.website {
                        <nav class="status-nav">
                            <a class="back-link" href=(url.as_str()) rel="noopener">"← " (text.as_str())</a>
                        </nav>
                    }
                    <header class="status-header grid-bg">
                        if let Some(logo) = &view.logo {
                            <img class="status-logo" src=(logo.as_str()) alt=(format!("{} logo", view.title))>
                        }
                        <div>
                            <h1>(view.title.as_str())</h1>
                            if let Some(description) = &view.description {
                                <p class="muted">(description.as_str())</p>
                            }
                        </div>
                    </header>
                    <nav class="status-tabs" aria-label="Status page sections">
                        for (which, label, href) in &tabs {
                            <a href=(href.as_str()) aria-current=((*which == tab).then_some("page"))>(*label)</a>
                        }
                    </nav>
                    (child)
                    <footer class="footer">
                        <a href=(format!("{base}/feed.atom")) rel="alternate" type="application/atom+xml">"Atom feed"</a>
                        " · "
                        <a href="https://github.com/mscno/uptimestatus">"Powered by uptimestatus"</a>
                    </footer>
                </div>
            </main>
        )
    })
}

/// The part of the page that refreshes: banner and components.
#[component]
async fn status_body(view: &StatusView, base: &str) -> Result<impl View> {
    let now = Timestamp::now();
    Ok(view! {
        <div id="status-body">
            <div class="banner" role="status" style=(format!("--pill: {}", status_color(view.status)))>
                <span>(view.status.headline())</span>
                <span class="spacer"></span>
                <span>"Updated " ago(at: view.updated_at, now: now)</span>
            </div>
            for window in &view.maintenances {
                <article class="notice" style="--pill: var(--maintenance)">
                    <header>
                        <span class="pill pill-maintenance">(if window.active { "In progress" } else { "Scheduled" })</span>
                        <strong>(window.title.as_str())</strong>
                    </header>
                    <p class="small muted">(fmt::when(window.starts_at)) " – " (fmt::when(window.ends_at))</p>
                    if let Some(description) = &window.description {
                        <p>(description.as_str())</p>
                    }
                </article>
            }
            for incident in view.incidents.iter().filter(|i| i.resolved_at.is_none()) {
                <article class="notice" style=(format!("--pill: {}", impact_color(incident.impact)))>
                    <header>
                        <span class=(incident_pill(incident.status, incident.impact))>(incident.status.label())</span>
                        <a class="notice-title" href=(format!("{base}/incidents/{}", incident.id))>(incident.title.as_str())</a>
                    </header>
                    <ol class="timeline">
                        for (status, body, at) in &incident.updates {
                            <li>
                                <span class="small muted">(status.label()) " · " (fmt::when(*at))</span>
                                <p>(body.as_str())</p>
                            </li>
                        }
                    </ol>
                </article>
            }
            for section in &view.sections {
                <section class="section">
                    <h2>(section.name.as_str())</h2>
                    for component in &section.components {
                        <div class="component">
                            <div class="component-head">
                                <strong>(component.name.as_str())</strong>
                                if let Some(uptime) = component.uptime {
                                    <span class="muted small">(fmt::uptime(uptime)) " uptime"</span>
                                }
                                state_pill(state: component.state)
                            </div>
                            <div class="bars" role="img" aria-label=(format!("{} uptime over the last {DAYS} days", component.name))>
                                for (day, level, uptime) in &component.bars {
                                    <span
                                        class=(bar_look(*level).0)
                                        data-day=(day.to_string())
                                        data-tone=(bar_look(*level).1)
                                        data-label=(bar_look(*level).2)
                                        data-uptime=(uptime.map(|u| format!("{} uptime", fmt::uptime(u))))
                                    ></span>
                                }
                            </div>
                            <div class="bar-legend"><span>"90 days ago"</span><span>"Today"</span></div>
                            uptime_windows(windows: &component.metrics.windows)
                            if component.metrics.svg.is_some() {
                                <details class="component-latency">
                                    <summary class="small muted">"Response time, last 24 hours"</summary>
                                    latency_chart(metrics: &component.metrics)
                                </details>
                            }
                        </div>
                    }
                </section>
            }
            if view.incidents.iter().any(|i| i.resolved_at.is_some()) {
                <section class="section">
                    <h2>"Past incidents"</h2>
                    <ul class="incidents">
                        for incident in view.incidents.iter().filter(|i| i.resolved_at.is_some()) {
                            <li>
                                <a class="notice-title" href=(format!("{base}/incidents/{}", incident.id))>(incident.title.as_str())</a>
                                <div class="small muted">
                                    (fmt::when(incident.started_at))
                                    if let Some(resolved) = incident.resolved_at { " · resolved " (fmt::when(resolved)) }
                                </div>
                                if let Some((_, body, _)) = incident.updates.first() {
                                    <p class="small">(body.as_str())</p>
                                }
                            </li>
                        }
                    </ul>
                    <p class="small"><a href=(format!("{base}/incidents"))>"All previous incidents →"</a></p>
                </section>
            }
        </div>
    })
}

/// Local signals for the bar hover card.
const BAR_TIP_SIGNALS: &str =
    "{_tip: {on: false, below: false, x: 0, y: 0, tone: 'none', label: '', uptime: '', date: ''}}";

/// Points the hover card at the bar under the pointer. Delegated from the
/// wrapper (`el`), so bars re-rendered by the live stream need nothing
/// re-bound. The card sits in the wrapper's coordinates (the frosted shell is
/// its containing block), above the bar, or below it near the viewport top.
/// Days are UTC dates; the date is spelled in the visitor's locale.
const BAR_TIP_SHOW: &str = "\
let b = evt.target.closest('.bars > [data-day]'); \
if (!b) { if (!evt.target.closest('.bars')) $_tip.on = false; return } \
let r = b.getBoundingClientRect(), box = el.getBoundingClientRect(), d = b.dataset; \
$_tip.below = r.top < 96; \
$_tip.x = Math.round(Math.min(Math.max(r.left + r.width / 2 - box.left, 96), box.width - 96)); \
$_tip.y = Math.round(($_tip.below ? r.bottom : r.top) - box.top); \
$_tip.tone = d.tone; \
$_tip.label = d.label; \
$_tip.uptime = d.uptime || ''; \
$_tip.date = new Date(d.day + 'T00:00:00Z').toLocaleDateString(undefined, {timeZone: 'UTC', month: 'short', day: '2-digit', year: 'numeric'}); \
$_tip.on = true";

/// The one hover card every bar shares (outside `#status-body`, so live
/// re-renders leave it alone).
#[component]
async fn bar_tip() -> Result<impl View> {
    Ok(view! {
        <div class="bar-tip" role="tooltip" aria-hidden="true" style="display: none"
            data-show="$_tip.on"
            data-class:below="$_tip.below"
            data-attr:data-tone="$_tip.tone"
            data-style:left="$_tip.x + 'px'"
            data-style:top="$_tip.y + 'px'">
            <div class="bar-tip-head">
                <span class="bar-tip-icon"></span>
                <strong data-text="$_tip.label"></strong>
                <span class="bar-tip-uptime" data-text="$_tip.uptime"></span>
            </div>
            <div class="bar-tip-date" data-text="$_tip.date"></div>
        </div>
    })
}

path_param!(slug);

#[page("/s/{slug}")]
pub(crate) async fn status_page(cx: &Cx) -> Result<impl View> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let base = base(cx, &view.slug);
    let live = format!("@get('{base}/live')");
    Ok(view! {
        ((header::CACHE_CONTROL, cache_control(&view)))
        frame(view: &view, base: &base, tab: Tab::Status, init: &live,
            <div class="status-now"
                data-signals=(BAR_TIP_SIGNALS)
                data-on:pointerover=(BAR_TIP_SHOW)
                data-on:pointerleave="$_tip.on = false">
                status_body(view: &view, base: &base)
                bar_tip()
            </div>
        )
    })
}

/// How often a live page refreshes without a check (incidents, maintenance,
/// and the "Updated" time).
const LIVE_TICK: Duration = Duration::from_secs(30);

/// Rebuilds the page (bypassing and then refreshing the cache) and renders
/// `#status-body`.
async fn fresh_body(cx: &Cx, slug: &str) -> Result<Event> {
    let state = app(cx);
    let view = Arc::new(build(&state.store, slug).await?.ok_or_else(not_found)?);
    if !view.published && current_admin(cx).await?.is_none() {
        return Err(not_found().into());
    }
    state.pages.put(view.clone());
    let base = base(cx, slug);
    let html = view! { cx => status_body(view: &view, base: &base) }
        .single()
        .await?
        .render(cx);
    Ok(PatchElements::new(html).into())
}

/// Live status page: re-renders as soon as one of its monitors is checked.
#[route(GET "/s/{slug}/live")]
pub(crate) async fn status_live(cx: &Cx) -> Result<Sse<impl Stream<Item = Result<Event>> + use<>>> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let slug = view.slug.clone();
    let monitors: HashSet<uptime_domain::MonitorId> = view.monitor_ids.iter().copied().collect();
    let cx = cx.clone();
    let receiver = app(&cx).events.subscribe();
    let tick = tokio::time::interval_at(tokio::time::Instant::now() + LIVE_TICK, LIVE_TICK);
    let events =
        futures_util::stream::unfold((receiver, tick, cx), move |(mut receiver, mut tick, cx)| {
            let (slug, monitors) = (slug.clone(), monitors.clone());
            async move {
                loop {
                    tokio::select! {
                        event = receiver.recv() => match event {
                            Ok(event) if monitors.contains(&event.monitor_id) => break,
                            Ok(_) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed) => return None,
                        },
                        _ = tick.tick() => break,
                    }
                }
                // Several checks landing together make one refresh.
                while receiver.try_recv().is_ok() {}
                let patch =
                    crate::logging::live_update(fresh_body(&cx, &slug).await, "status page");
                Some((patch, (receiver, tick, cx)))
            }
        });
    Ok(Sse::new(events).keep_alive(KeepAlive::new()))
}

/// Datastar refresh: re-renders `#status-body`.
#[route(GET "/s/{slug}/body")]
pub(crate) async fn status_body_patch(cx: &Cx) -> Result<PatchElements> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let base = base(cx, &view.slug);
    let html = view! { cx => status_body(view: &view, base: &base) }
        .single()
        .await?
        .render(cx);
    Ok(PatchElements::new(html))
}

#[derive(Serialize)]
pub(crate) struct Summary {
    page: SummaryPage,
    status: PageStatus,
    updated_at: Timestamp,
    components: Vec<SummaryComponent>,
    incidents: Vec<SummaryIncident>,
    maintenances: Vec<SummaryMaintenance>,
}

#[derive(Serialize)]
struct SummaryIncident {
    title: String,
    impact: Impact,
    status: IncidentStatus,
    started_at: Timestamp,
    resolved_at: Option<Timestamp>,
}

#[derive(Serialize)]
struct SummaryMaintenance {
    title: String,
    starts_at: Timestamp,
    ends_at: Timestamp,
    active: bool,
}

#[derive(Serialize)]
struct SummaryPage {
    slug: String,
    title: String,
}

#[derive(Serialize)]
struct SummaryComponent {
    section: String,
    name: String,
    status: MonitorState,
    uptime_90d: Option<f64>,
}

/// Machine-readable current status.
#[route(GET "/s/{slug}/summary.json")]
pub(crate) async fn summary_json(cx: &Cx) -> Result<Json<Summary>> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    Ok(Json(Summary {
        page: SummaryPage {
            slug: view.slug.clone(),
            title: view.title.clone(),
        },
        status: view.status,
        updated_at: view.updated_at,
        components: view
            .sections
            .iter()
            .flat_map(|section| {
                section.components.iter().map(|component| SummaryComponent {
                    section: section.name.clone(),
                    name: component.name.clone(),
                    status: component.state,
                    uptime_90d: component.uptime,
                })
            })
            .collect(),
        incidents: view
            .incidents
            .iter()
            .map(|i| SummaryIncident {
                title: i.title.clone(),
                impact: i.impact,
                status: i.status,
                started_at: i.started_at,
                resolved_at: i.resolved_at,
            })
            .collect(),
        maintenances: view
            .maintenances
            .iter()
            .map(|m| SummaryMaintenance {
                title: m.title.clone(),
                starts_at: m.starts_at,
                ends_at: m.ends_at,
                active: m.active,
            })
            .collect(),
    }))
}
