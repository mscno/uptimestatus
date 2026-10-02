//! The admin console: monitors dashboard, CRUD, live updates and "Test now".

mod alerts;
mod bulk;
mod domains;
pub mod form;
mod images;
mod incidents;
pub mod page_form;
mod pages;
mod tokens;
mod views;

use std::time::Duration;

use futures_util::Stream;
use jiff::Timestamp;
use tokio::sync::broadcast::error::RecvError;
use topcoat::{
    Result,
    context::Cx,
    datastar::PatchElements,
    router::{
        StatusCode,
        content::{
            Form, Html,
            sse::{Event, KeepAlive, Sse},
        },
        error::{SeeOther, not_found, see_other},
        page, path_param,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{CheckSpec, Health, MonitorId, MonitorSpec, MonitorState, evaluate};
use uptime_store::{Monitor, StoreError};

use self::{
    form::{FormErrors, MonitorForm},
    views::{monitor_form, monitor_row, summary, target},
};
use crate::{
    AppState,
    auth::{app, require_admin},
    fmt,
    metrics::{Metrics, Range, latency_chart, uptime_windows},
    views::{admin_shell, ago, delete_button, health_pill, state_pill},
};

path_param!(id: i64, error = not_found);

/// `?range=24h|7d`: how much history the latency chart covers.
#[topcoat::router::query_params]
pub(crate) struct ChartQuery {
    range: Option<String>,
}

/// `?from=<id>`: the record a "new" form is duplicated from.
#[topcoat::router::query_params]
pub(crate) struct FromQuery {
    from: Option<String>,
}

/// The id in `?from=`, if any.
pub(crate) fn copied_from(cx: &Cx) -> Option<i64> {
    topcoat::router::query_params::<FromQuery>(cx)
        .ok()
        .and_then(|q| q.from.as_deref()?.trim().parse().ok())
}

/// `base-copy`, or `base-copy-2`, `-3`, … : the first not in `taken`, at most
/// `max_len` characters (the base is shortened to fit).
pub(crate) fn copy_of(base: &str, taken: &[String], max_len: usize) -> String {
    (1..)
        .map(|n| {
            let suffix = if n == 1 {
                "-copy".to_owned()
            } else {
                format!("-copy-{n}")
            };
            let keep = max_len.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| format!("{base}-copy"))
}

pub(crate) use alerts::{
    alerts_page, create_channel, delete_channel, edit_channel, new_channel, test_channel,
    update_channel,
};
pub(crate) use bulk::bulk_action;
pub(crate) use domains::{add_domain, remove_domain, verify_domain};
pub(crate) use images::{remove_image, upload_image};
pub(crate) use incidents::{
    create_incident, create_maintenance, delete_incident, delete_maintenance, edit_maintenance,
    incident_detail, incidents_page, maintenance_page, new_incident, new_maintenance,
    post_incident_update, update_maintenance,
};
pub(crate) use pages::{
    create_page, delete_page, edit_page, export_toml, new_page, pages_list, update_page,
};
pub(crate) use tokens::{api_page, create_token, revoke_token};

fn monitor_id(cx: &Cx) -> Result<MonitorId> {
    Ok(MonitorId(*path_param::<Id>(cx)?))
}

async fn load_monitor(state: &AppState, id: MonitorId) -> Result<Monitor> {
    Ok(state.store.monitor(id).await?.ok_or_else(not_found)?)
}

// ── Dashboard ────────────────────────────────────────────────────────────

#[page("/admin")]
pub(crate) async fn dashboard(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let overviews = app(cx).store.monitor_overviews().await?;
    let tags = all_tags(&overviews);
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Monitors", admin: &admin, section: "monitors",
            <div class="page-head">
                <h1>"Monitors"</h1>
                <span class="spacer"></span>
                <a class="btn btn-primary" href="/admin/monitors/new">"New monitor"</a>
            </div>
            <div id="live" data-init="@get('/admin/live')"></div>
            summary(overviews: &overviews)
            if overviews.is_empty() {
                <div class="card empty">
                    <p><strong>"No monitors yet."</strong></p>
                    <p>"Create one, or load a file with " <code>"uptimestatus seed monitors.toml"</code> "."</p>
                </div>
            } else {
                <div data-signals=(DASHBOARD_SIGNALS)>
                    dashboard_tools(tags: &tags)
                    monitors_table(overviews: &overviews, now: now)
                </div>
            }
        )
    })
}

/// Client-side state: the selection, the search text, the tag filter and the
/// pending bulk action.
const DASHBOARD_SIGNALS: &str = "{sel: [], q: '', tagf: '', bulk_action: '', bulk_tag: ''}";

/// Every tag in use, sorted.
fn all_tags(overviews: &[uptime_store::MonitorOverview]) -> Vec<String> {
    let mut tags: Vec<String> = overviews
        .iter()
        .flat_map(|o| o.monitor.spec.tags.iter().cloned())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// Posts the chosen bulk action with the selection.
fn bulk_click(action: &str, confirm: &str) -> String {
    let guard = if confirm.is_empty() {
        String::new()
    } else {
        format!("if (!confirm({confirm})) return; ")
    };
    format!("{guard}$bulk_action = '{action}'; @post('/admin/monitors/bulk')")
}

/// Search box, tag filter and the bulk-action bar (shown while monitors are selected).
#[component]
async fn dashboard_tools(tags: &[String]) -> Result<impl View> {
    let delete = bulk_click(
        "delete",
        "'Delete ' + $sel.length + ' monitor(s) and their history?'",
    );
    let pause = bulk_click("pause", "");
    let resume = bulk_click("resume", "");
    let tag = bulk_click("tag", "");
    let untag = bulk_click("untag", "");
    Ok(view! {
        <div class="tools">
            <input type="search" class="search" placeholder="Search monitors" aria-label="Search monitors" data-bind:q="">
            if !tags.is_empty() {
                <ul class="chips" aria-label="Filter by tag">
                    for tag in tags {
                        <li>
                            <button type="button" class="chip"
                                data-class:active=(format!("$tagf == '{tag}'"))
                                data-on:click=(format!("$tagf = $tagf == '{tag}' ? '' : '{tag}'"))>(tag.as_str())</button>
                        </li>
                    }
                </ul>
            }
        </div>
        <div class="bulkbar card" style="display: none" data-show="$sel.length > 0">
            <strong data-text="$sel.length + ' selected'"></strong>
            <button class="btn" type="button" data-on:click=(pause)>"Pause"</button>
            <button class="btn" type="button" data-on:click=(resume)>"Resume"</button>
            <input type="text" class="bulk-tag" placeholder="tag" aria-label="Tag for bulk tagging" data-bind:bulk_tag="">
            <button class="btn" type="button" data-on:click=(tag)>"Add tag"</button>
            <button class="btn" type="button" data-on:click=(untag)>"Remove tag"</button>
            <button class="btn btn-danger" type="button" data-on:click=(delete)>"Delete"</button>
            <button class="btn btn-link" type="button" data-on:click="$sel = []">"Clear"</button>
            <span id="bulk-result"></span>
        </div>
    })
}

/// One line of the dashboard: a group header or a monitor, at some depth.
enum DashRow<'a> {
    Group {
        path: String,
        name: String,
        depth: usize,
        state: MonitorState,
        count: usize,
    },
    Monitor {
        overview: &'a uptime_store::MonitorOverview,
        depth: usize,
    },
}

impl DashRow<'_> {
    /// A stable identity for live morphing.
    fn key(&self) -> String {
        match self {
            Self::Group { path, .. } => format!("g:{path}"),
            Self::Monitor { overview, .. } => format!("m:{}", overview.monitor.id.0),
        }
    }
}

/// Ungrouped monitors first, then each group (a header, its monitors, its
/// subgroups), depth first.
fn dash_rows(overviews: &[uptime_store::MonitorOverview]) -> Vec<DashRow<'_>> {
    use uptime_domain::group::{Node, tree};

    fn walk<'a>(
        node: &Node<&'a uptime_store::MonitorOverview>,
        depth: usize,
        rows: &mut Vec<DashRow<'a>>,
    ) {
        let all = node.all_items();
        rows.push(DashRow::Group {
            path: node.path.clone(),
            name: node.name.clone(),
            depth,
            state: uptime_domain::rollup(all.iter().map(|o| o.runtime.runtime.state)),
            count: all.len(),
        });
        for overview in &node.items {
            rows.push(DashRow::Monitor {
                overview,
                depth: depth + 1,
            });
        }
        for child in &node.children {
            walk(child, depth + 1, rows);
        }
    }

    let (ungrouped, roots) = tree(overviews.iter().map(|o| (o.monitor.spec.group.clone(), o)));
    let mut rows: Vec<DashRow<'_>> = ungrouped
        .into_iter()
        .map(|overview| DashRow::Monitor { overview, depth: 0 })
        .collect();
    for root in &roots {
        walk(root, 0, &mut rows);
    }
    rows
}

/// The dashboard table. Patched whole (a lone `<tr>` cannot be parsed
/// outside a table); morphing leaves unchanged rows alone.
#[component]
async fn monitors_table(
    overviews: &[uptime_store::MonitorOverview],
    now: Timestamp,
) -> Result<impl View> {
    let rows = dash_rows(overviews);
    Ok(view! {
        <div id="monitors" class="card table-wrap">
            <table class="list">
                <thead>
                    <tr>
                        <th class="select">
                            <input type="checkbox" aria-label="Select all shown monitors"
                                data-on:change="$sel = evt.target.checked ? [...document.querySelectorAll('#monitors tr[data-id]')].filter(r => r.style.display !== 'none').map(r => r.dataset.id) : []">
                        </th>
                        <th>"State"</th><th>"Monitor"</th><th class="hide-sm">"Target"</th>
                        <th class="num">"Latency"</th><th class="hide-sm">"Last check"</th><th class="hide-sm">"Next"</th><th></th>
                    </tr>
                </thead>
                <tbody>
                    #[key(row.key())]
                    for row in &rows {
                        if let DashRow::Group { path, name, depth, state, count } = row {
                            <tr class="group-row" data-group=(path.as_str()) data-show="!$q && !$tagf"
                                style=(format!("--depth: {depth}"))>
                                <td></td>
                                <td>state_pill(state: *state)</td>
                                <td class="name-cell" colspan="6">
                                    <strong class="group-name">(name.as_str())</strong>
                                    <span class="muted small">" " (count.to_string()) if *count == 1 { " monitor" } else { " monitors" }</span>
                                </td>
                            </tr>
                        } else if let DashRow::Monitor { overview, depth } = row {
                            monitor_row(overview: overview, now: now, depth: *depth)
                        }
                    }
                </tbody>
            </table>
        </div>
    })
}

/// Live dashboard: after each check, morph that monitor's row and the summary.
#[route(GET "/admin/live")]
pub(crate) async fn live(cx: &Cx) -> Result<Sse<impl Stream<Item = Result<Event>> + use<>>> {
    require_admin(cx).await?;
    let cx = cx.clone();
    let receiver = app(&cx).events.subscribe();
    let events = futures_util::stream::unfold((receiver, cx), |(mut receiver, cx)| async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let patch = crate::logging::live_update(
                        row_patch(&cx, event.monitor_id).await,
                        "dashboard",
                    );
                    return Some((patch, (receiver, cx)));
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new()))
}

async fn row_patch(cx: &Cx, _id: MonitorId) -> Result<Event> {
    let overviews = app(cx).store.monitor_overviews().await?;
    let now = Timestamp::now();
    let html = view! { cx =>
        summary(overviews: &overviews)
        if !overviews.is_empty() {
            monitors_table(overviews: &overviews, now: now)
        }
    }
    .single()
    .await?
    .render(cx);
    Ok(PatchElements::new(html).into())
}

// ── Create / edit ────────────────────────────────────────────────────────

#[page("/admin/monitors/new")]
pub(crate) async fn new_monitor(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let store = &app(cx).store;
    let channels = store.list_channels().await?;
    let (title, form) = match copied_from(cx) {
        Some(id) => {
            let source = load_monitor(app(cx), MonitorId(id)).await?;
            let taken: Vec<String> = store
                .list_monitors()
                .await?
                .into_iter()
                .map(|m| m.spec.key.to_string())
                .collect();
            let form = MonitorForm::duplicate(&source.spec, &taken)
                .with_channels(&store.monitor_channels(source.id).await?);
            (format!("Duplicate {}", source.spec.name), form)
        }
        None => (
            "New monitor".to_owned(),
            MonitorForm::new_monitor().with_channels(&store.default_channels().await?),
        ),
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: &title, admin: &admin, section: "monitors",
            <div class="page-head"><h1>(title.as_str())</h1></div>
            <div class="card">monitor_form(form: &form, errors: &errors, action: "/admin/monitors", cancel: "/admin", channels: &channels)</div>
        )
    })
}

#[route(POST "/admin/monitors")]
pub(crate) async fn create_monitor(cx: &Cx, Form(input): Form<MonitorForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let errors = match input.to_spec() {
        Ok(spec) => {
            let first_run =
                uptime_runtime::schedule::first_run_at(Timestamp::now(), &spec.key, Duration::ZERO);
            match state.store.create_monitor(&spec, first_run).await {
                Ok(monitor) => {
                    state
                        .store
                        .set_monitor_channels(monitor.id, &input.channel_ids())
                        .await?;
                    state.wake_scheduler();
                    tracing::info!(monitor = %monitor.id, key = %spec.key, by = %admin.login, "monitor created");
                    return see_other(format!("/admin/monitors/{}", monitor.id)).into_response(cx);
                }
                Err(StoreError::DuplicateKey(key)) => duplicate_key(&key.to_string()),
                Err(error) => return Err(error.into()),
            }
        }
        Err(errors) => errors,
    };
    form_page(
        cx,
        &admin,
        "New monitor",
        &input,
        &errors,
        "/admin/monitors",
        "/admin",
    )
    .await
}

#[page("/admin/monitors/{id}/edit")]
pub(crate) async fn edit_monitor(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let id = monitor_id(cx)?;
    let monitor = load_monitor(app(cx), id).await?;
    let store = &app(cx).store;
    let channels = store.list_channels().await?;
    let form =
        MonitorForm::from_spec(&monitor.spec).with_channels(&store.monitor_channels(id).await?);
    let errors = FormErrors::default();
    let action = format!("/admin/monitors/{id}");
    Ok(view! {
        admin_shell(title: "Edit monitor", admin: &admin, section: "monitors",
            <div class="page-head"><h1>"Edit " (monitor.spec.name.as_str())</h1></div>
            <div class="card">monitor_form(form: &form, errors: &errors, action: &action, cancel: &action, channels: &channels)</div>
        )
    })
}

#[route(POST "/admin/monitors/{id}")]
pub(crate) async fn update_monitor(cx: &Cx, Form(input): Form<MonitorForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let id = monitor_id(cx)?;
    let state = app(cx);
    let action = format!("/admin/monitors/{id}");
    let errors = match input.to_spec() {
        Ok(spec) => match state
            .store
            .update_monitor(id, &spec, Timestamp::now())
            .await
        {
            Ok(_) => {
                state
                    .store
                    .set_monitor_channels(id, &input.channel_ids())
                    .await?;
                state.wake_scheduler();
                tracing::info!(monitor = %id, by = %admin.login, "monitor updated");
                return see_other(&action).into_response(cx);
            }
            Err(StoreError::DuplicateKey(key)) => duplicate_key(&key.to_string()),
            Err(StoreError::MonitorNotFound(_)) => return Err(not_found().into()),
            Err(error) => return Err(error.into()),
        },
        Err(errors) => errors,
    };
    form_page(
        cx,
        &admin,
        "Edit monitor",
        &input,
        &errors,
        &action,
        &action,
    )
    .await
}

fn duplicate_key(key: &str) -> FormErrors {
    let mut errors = FormErrors::default();
    errors.insert(
        "key",
        format!("Another monitor already uses the key `{key}`."),
    );
    errors
}

/// Re-renders a submitted form with its errors (422).
async fn form_page(
    cx: &Cx,
    admin: &uptime_store::AdminUser,
    title: &str,
    input: &MonitorForm,
    errors: &FormErrors,
    action: &str,
    cancel: &str,
) -> Result<Response> {
    let channels = app(cx).store.list_channels().await?;
    let html = view! { cx =>
        admin_shell(title: title, admin: admin, section: "monitors",
            <div class="page-head"><h1>(title)</h1></div>
            <div class="card">monitor_form(form: input, errors: errors, action: action, cancel: cancel, channels: &channels)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

// ── Detail and actions ───────────────────────────────────────────────────

#[page("/admin/monitors/{id}")]
pub(crate) async fn monitor_detail(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let id = monitor_id(cx)?;
    let state = app(cx);
    let monitor = load_monitor(state, id).await?;
    let runtime = state.store.runtime(id).await?.ok_or_else(not_found)?;
    let checks = state.store.recent_checks(id, 50).await?;
    let now = Timestamp::now();
    let range = topcoat::router::query_params::<ChartQuery>(cx)
        .ok()
        .and_then(|q| Range::parse(q.range.as_deref()?))
        .unwrap_or_default();
    let metrics = Metrics::load(&state.store, id, range, now).await?;
    let spec = &monitor.spec;
    let policy = &spec.policy;
    let paused = !spec.active;
    let base = format!("/admin/monitors/{id}");
    let delete_url = format!("{base}/delete");
    let details: Vec<(&str, String)> = match &spec.check {
        CheckSpec::Http(http) => vec![
            ("Request", format!("{} {}", http.method.as_str(), http.url)),
            ("Accepted", http.accepted_status.to_string()),
            (
                "Keyword",
                http.keyword.as_ref().map_or("—".into(), |k| {
                    format!(
                        "{} \"{}\"",
                        if k.absent {
                            "must not contain"
                        } else {
                            "must contain"
                        },
                        k.text
                    )
                }),
            ),
            (
                "JSON",
                http.json
                    .as_ref()
                    .map_or("—".into(), |rule| match &rule.expect {
                        Some(expect) => format!("{} = {expect}", rule.path),
                        None => format!("{} exists", rule.path),
                    }),
            ),
            (
                "Auth",
                match &http.auth {
                    Some(uptime_domain::HttpAuth::Basic { username, .. }) => {
                        format!("basic ({username})")
                    }
                    Some(uptime_domain::HttpAuth::Bearer { .. }) => "bearer token".into(),
                    None => "—".into(),
                },
            ),
            ("Redirects", http.max_redirects.to_string()),
        ],
        CheckSpec::Tcp(tcp) => {
            let mut rows = vec![(
                "Connect",
                format!(
                    "{}:{}{}",
                    tcp.host,
                    tcp.port,
                    if tcp.tls { " (TLS)" } else { "" }
                ),
            )];
            if let Some(send) = &tcp.send {
                rows.push(("Send", send.clone()));
            }
            if let Some(expect) = &tcp.expect {
                rows.push(("Expect", expect.clone()));
            }
            rows
        }
        CheckSpec::Dns(dns) => vec![
            ("Lookup", format!("{} {}", dns.record_type, dns.name)),
            (
                "Resolver",
                dns.resolver.map_or("system".into(), |ip| ip.to_string()),
            ),
            ("Expect", dns.expect.clone().unwrap_or_else(|| "—".into())),
        ],
        CheckSpec::Push(push) => vec![(
            "Push URL",
            state
                .hosts
                .app_url()
                .join(&format!("/api/push/{}", push.token))
                .map_or_else(
                    |_| format!("/api/push/{}", push.token),
                    |url| url.to_string(),
                ),
        )],
    };
    let push_url = details
        .iter()
        .find(|(label, _)| *label == "Push URL")
        .map(|(_, url)| url.clone());
    let schedule = vec![
        ("Interval", fmt::duration(policy.interval)),
        ("Timeout", fmt::duration(policy.timeout)),
        (
            "Retries",
            format!(
                "{} (every {})",
                policy.retries,
                fmt::duration(policy.retry_interval)
            ),
        ),
        (
            "Degraded after",
            policy.degraded_after.map_or("—".into(), fmt::duration),
        ),
        (
            "Upside-down",
            if policy.invert {
                "yes".into()
            } else {
                "no".into()
            },
        ),
    ];
    Ok(view! {
        admin_shell(title: &monitor.spec.name, admin: &admin, section: "monitors",
            <div class="page-head">
                <h1>(monitor.spec.name.as_str())</h1>
                monitor_state(state: runtime.runtime.state)
                <span class="spacer"></span>
                <a class="btn" href=(format!("{base}/edit"))>"Edit"</a>
                <a class="btn" href=(format!("/admin/monitors/new?from={id}"))>"Duplicate"</a>
                <form method="post" action=(format!("{base}/{}", if paused { "resume" } else { "pause" }))>
                    <button class="btn" type="submit">(if paused { "Resume" } else { "Pause" })</button>
                </form>
                delete_button(action: &delete_url, heading: "Delete this monitor?",
                    body: "Its check history goes too, and it disappears from every status page.")
            </div>

            <div class="cols">
                <div class="card">
                    <h2>"Check"</h2>
                    if let Some(url) = &push_url {
                        <div class="row small" style="margin-bottom: 1rem">
                            <code>(url.as_str())</code>
                            <sb-copy-button value=(url.as_str()) label="Copy push URL"></sb-copy-button>
                        </div>
                    }
                    <dl class="facts">
                        <dt>"Key"</dt><dd class="mono">(monitor.spec.key.as_str())</dd>
                        for (label, value) in details {
                            <dt>(label)</dt><dd class="mono small">(value)</dd>
                        }
                        for (label, value) in schedule {
                            <dt>(label)</dt><dd>(value)</dd>
                        }
                    </dl>
                </div>
                <div class="card">
                    <h2>"Right now"</h2>
                    monitor_now(runtime: &runtime, checks: &checks, paused: paused, now: now)
                    <div class="actions">
                        <button class="btn" type="button"
                            data-on:click=(format!("@post('{base}/test')"))
                            data-indicator:_testing="" data-attr:aria-busy="$_testing">"Test now"</button>
                    </div>
                    <div id="test-result" class="stack"></div>
                </div>
            </div>

            <div class="card">
                <div class="row">
                    <h2>"Response time"</h2>
                    <span class="spacer"></span>
                    <nav class="range-tabs" aria-label="Range">
                        for choice in [Range::Day, Range::Week] {
                            <a href=(format!("{base}?range={}", choice.as_str()))
                                aria-current=((choice == range).then_some("true"))>(choice.as_str())</a>
                        }
                    </nav>
                </div>
                latency_chart(metrics: &metrics)
                <h3 class="label">"Uptime"</h3>
                uptime_windows(windows: &metrics.windows)
            </div>

            recent_checks(checks: &checks, now: now)
            <div data-init=(format!("@get('{base}/live')"))></div>
        )
    })
}

/// The state pill in the detail page head (patched live).
#[component]
async fn monitor_state(state: MonitorState) -> Result<impl View> {
    Ok(view! { <span id="monitor-state">state_pill(state: state)</span> })
}

/// The "Right now" facts and latency trend (patched live).
#[component]
async fn monitor_now(
    runtime: &uptime_store::RuntimeSnapshot,
    checks: &[uptime_store::StoredCheck],
    paused: bool,
    now: Timestamp,
) -> Result<impl View> {
    // Oldest first, for the sparkline.
    let latencies: Vec<i64> = checks.iter().rev().filter_map(|c| c.latency_ms).collect();
    let latencies_json = serde_json::to_string(&latencies).unwrap_or_else(|_| "[]".into());
    Ok(view! {
        <div id="monitor-now">
            <dl class="facts">
                <dt>"Last check"</dt>
                <dd>
                    if let Some(at) = runtime.last_checked_at { ago(at: at, now: now) } else { "never" }
                </dd>
                <dt>"Latency"</dt><dd>(runtime.last_latency_ms.map_or("—".into(), |ms| format!("{ms}ms")))</dd>
                <dt>"Status code"</dt><dd>(runtime.last_status_code.map_or("—".into(), |c| c.to_string()))</dd>
                <dt>"Error"</dt><dd class="small">(runtime.last_error.clone().unwrap_or_else(|| "—".into()))</dd>
                <dt>"State since"</dt>
                <dd>
                    if let Some(at) = runtime.state_changed_at { ago(at: at, now: now) } else { "—" }
                </dd>
                if let Some(expires) = runtime.cert_expires_at {
                    <dt>"Certificate"</dt>
                    <dd class=(if uptime_domain::cert::days_left(expires, now) < 14 { "small error-text" } else { "small" })>
                        "expires " (fmt::when(expires)) " (" (format!("{} days", uptime_domain::cert::days_left(expires, now))) ")"
                    </dd>
                }
                <dt>"Next check"</dt>
                <dd>
                    if paused { "paused" } else { ago(at: runtime.next_run_at, now: now) }
                </dd>
            </dl>
            if !latencies.is_empty() {
                <div class="trend">
                    <span class="small muted">"Latency"</span>
                    <sb-sparkline values=(latencies_json.clone()) length=(latencies.len().to_string())
                        tone="brand" unit="ms" show-value="" label="Response time, recent checks"></sb-sparkline>
                </div>
            }
        </div>
    })
}

/// The recent checks table (patched live; new checks appear on top).
#[component]
async fn recent_checks(checks: &[uptime_store::StoredCheck], now: Timestamp) -> Result<impl View> {
    Ok(view! {
        <div id="recent-checks" class="card table-wrap">
            <h2>"Recent checks"</h2>
            if checks.is_empty() {
                <p class="muted">"No checks yet."</p>
            } else {
                <table class="list">
                    <thead><tr><th>"When"</th><th>"Result"</th><th class="num">"Latency"</th><th class="hide-sm">"Code"</th><th class="hide-sm">"Region"</th><th>"Error"</th></tr></thead>
                    <tbody>
                        for check in checks {
                            <tr>
                                <td class="small when">ago(at: check.checked_at, now: now)</td>
                                <td>health_pill(health: check.health)</td>
                                <td class="num">(check.latency_ms.map_or("—".into(), |ms| format!("{ms}ms")))</td>
                                <td class="hide-sm">(check.status_code.map_or("—".into(), |c| c.to_string()))</td>
                                <td class="small muted hide-sm">(check.region.as_str())</td>
                                <td class="small">(check.error.clone().unwrap_or_default())</td>
                            </tr>
                        }
                    </tbody>
                </table>
            }
        </div>
    })
}

/// How often live pages refresh without an event (keeps relative times current).
const LIVE_TICK: Duration = Duration::from_secs(30);

/// The monitor page's live parts, freshly loaded.
async fn detail_patch(cx: &Cx, id: MonitorId) -> Result<Event> {
    let state = app(cx);
    let monitor = load_monitor(state, id).await?;
    let runtime = state.store.runtime(id).await?.ok_or_else(not_found)?;
    let checks = state.store.recent_checks(id, 50).await?;
    let now = Timestamp::now();
    let html = view! { cx =>
        monitor_state(state: runtime.runtime.state)
        monitor_now(runtime: &runtime, checks: &checks, paused: !monitor.spec.active, now: now)
        recent_checks(checks: &checks, now: now)
    }
    .single()
    .await?
    .render(cx);
    Ok(PatchElements::new(html).into())
}

/// Live monitor page: re-renders as soon as this monitor is checked.
#[route(GET "/admin/monitors/{id}/live")]
pub(crate) async fn monitor_live(
    cx: &Cx,
) -> Result<Sse<impl Stream<Item = Result<Event>> + use<>>> {
    require_admin(cx).await?;
    let id = monitor_id(cx)?;
    let cx = cx.clone();
    let receiver = app(&cx).events.subscribe();
    let tick = tokio::time::interval_at(tokio::time::Instant::now() + LIVE_TICK, LIVE_TICK);
    let events = futures_util::stream::unfold(
        (receiver, tick, cx),
        move |(mut receiver, mut tick, cx)| async move {
            loop {
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) if event.monitor_id == id => break,
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return None,
                    },
                    _ = tick.tick() => break,
                }
            }
            let patch = crate::logging::live_update(detail_patch(&cx, id).await, "monitor");
            Some((patch, (receiver, tick, cx)))
        },
    );
    Ok(Sse::new(events).keep_alive(KeepAlive::new()))
}

#[route(POST "/admin/monitors/{id}/pause")]
pub(crate) async fn pause_monitor(cx: &Cx) -> Result<SeeOther> {
    set_active(cx, false).await
}

#[route(POST "/admin/monitors/{id}/resume")]
pub(crate) async fn resume_monitor(cx: &Cx) -> Result<SeeOther> {
    set_active(cx, true).await
}

async fn set_active(cx: &Cx, active: bool) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = monitor_id(cx)?;
    let state = app(cx);
    match state
        .store
        .set_monitor_active(id, active, Timestamp::now())
        .await
    {
        Ok(()) => {}
        Err(StoreError::MonitorNotFound(_)) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    }
    if active {
        state.wake_scheduler();
    }
    tracing::info!(monitor = %id, active, by = %admin.login, "monitor toggled");
    Ok(see_other(format!("/admin/monitors/{id}")))
}

#[route(POST "/admin/monitors/{id}/delete")]
pub(crate) async fn delete_monitor(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = monitor_id(cx)?;
    if !app(cx).store.delete_monitor(id).await? {
        return Err(not_found().into());
    }
    tracing::info!(monitor = %id, by = %admin.login, "monitor deleted");
    Ok(see_other("/admin"))
}

// ── Test now ─────────────────────────────────────────────────────────────

/// Runs the unsaved form once and patches `#test-result`.
#[route(POST "/admin/monitors/test")]
pub(crate) async fn test_unsaved(cx: &Cx, Form(input): Form<MonitorForm>) -> Result<PatchElements> {
    require_admin(cx).await?;
    let html = match input.to_spec() {
        Ok(spec) => test_result(cx, &spec).await?,
        Err(errors) => {
            let fields = errors.fields().collect::<Vec<_>>().join(", ");
            result_panel(
                cx,
                "var(--down)",
                "Fix the highlighted fields first",
                &fields,
            )
            .await?
        }
    };
    Ok(PatchElements::new(html))
}

/// Runs a saved monitor once and patches `#test-result`.
#[route(POST "/admin/monitors/{id}/test")]
pub(crate) async fn test_saved(cx: &Cx) -> Result<PatchElements> {
    require_admin(cx).await?;
    let monitor = load_monitor(app(cx), monitor_id(cx)?).await?;
    Ok(PatchElements::new(test_result(cx, &monitor.spec).await?))
}

async fn test_result(cx: &Cx, spec: &MonitorSpec) -> Result<String> {
    let Some(prober) = &app(cx).prober else {
        return result_panel(cx, "var(--unknown)", "Probing is not available", "").await;
    };
    let observation = prober.probe(&spec.check, spec.policy.timeout).await;
    let verdict = evaluate(&spec.check, &spec.policy, &observation);
    let color = match verdict.health {
        Health::Up => "var(--up)",
        Health::Degraded => "var(--degraded)",
        Health::Down => "var(--down)",
    };
    let headline = format!(
        "{} — {}",
        crate::views::health_label(verdict.health),
        target(&spec.check)
    );
    let mut facts = Vec::new();
    if let Some(code) = verdict.status_code {
        facts.push(format!("HTTP {code}"));
    }
    if let Some(latency) = verdict.latency {
        facts.push(format!("{}ms", latency.as_millis()));
    }
    if let Some(reason) = &verdict.reason {
        facts.push(reason.to_string());
    }
    result_panel(cx, color, &headline, &facts.join(" · ")).await
}

async fn result_panel(cx: &Cx, color: &str, headline: &str, detail: &str) -> Result<String> {
    Ok(view! { cx =>
        <div id="test-result" class="stack">
            <div class="result" style=(format!("--pill: {color}"))>
                <strong>(headline)</strong>
                if !detail.is_empty() {
                    <div class="small">(detail)</div>
                }
            </div>
        </div>
    }
    .single()
    .await?
    .render(cx))
}
