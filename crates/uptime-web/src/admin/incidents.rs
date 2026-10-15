//! Incidents (declare, post updates, resolve) and maintenance windows.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        page, path_param,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{
    Health, Impact, IncidentKind, IncidentStatus, MaintenanceSpec, MonitorKey, Repeat,
};
use uptime_store::{
    AdminUser, Incident, Monitor, NewIncident, NewScheduledUpdate, ScheduledKind, ScheduledUpdate,
    StoreError, StoredCheck, Template,
};

use super::{
    Id,
    form::FormErrors,
    templates::template_picker,
    views::{checkbox, target, text_area, text_field},
};
use crate::{
    audit,
    auth::{app, require_admin},
    fmt,
    publish::{self, Via},
    views::{admin_shell, ago, delete_button, health_pill, state_pill},
};

pub(super) fn path_id(cx: &Cx) -> Result<i64> {
    Ok(*path_param::<Id>(cx)?)
}

/// `monitor_<key>=on` toggles, sent along with the rest of a form.
fn chosen_monitors(extra: &BTreeMap<String, String>) -> Vec<MonitorKey> {
    extra
        .iter()
        .filter(|(_, value)| value.as_str() == "on")
        .filter_map(|(name, _)| name.strip_prefix("monitor_")?.parse().ok())
        .collect()
}

pub(super) fn impact_pill(impact: Impact) -> &'static str {
    match impact {
        Impact::None => "pill pill-maintenance",
        Impact::Minor => "pill pill-degraded",
        Impact::Major | Impact::Critical => "pill pill-down",
    }
}

pub(super) fn status_pill(status: IncidentStatus) -> &'static str {
    match status {
        IncidentStatus::Resolved => "pill pill-up",
        IncidentStatus::Monitoring => "pill pill-maintenance",
        IncidentStatus::Identified | IncidentStatus::Investigating => "pill pill-pending",
    }
}

/// Toggles for every monitor, `chosen` switched on.
#[component]
async fn monitor_toggles(
    monitors: &[Monitor],
    chosen: &[MonitorKey],
    errors: &FormErrors,
) -> Result<impl View> {
    let toggles: Vec<(String, String, bool)> = monitors
        .iter()
        .map(|m| {
            (
                format!("monitor_{}", m.spec.key),
                format!("{} ({})", m.spec.name, m.spec.key),
                chosen.contains(&m.spec.key),
            )
        })
        .collect();
    Ok(view! {
        <fieldset>
            <legend>"Affected monitors"</legend>
            if let Some(error) = errors.get("monitors") {
                <p class="flash flash-error">(error.to_owned())</p>
            }
            if toggles.is_empty() {
                <p class="muted small">"No monitors yet."</p>
            }
            for (name, label, checked) in &toggles {
                checkbox(name: name, label: label, checked: *checked)
            }
        </fieldset>
    })
}

// ── Incidents ────────────────────────────────────────────────────────────

#[page("/admin/incidents")]
pub(crate) async fn incidents_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let incidents = app(cx).store.list_incidents(100).await?;
    let scheduled = app(cx).store.scheduled_declarations().await?;
    let rows: Vec<_> = incidents
        .into_iter()
        .map(|incident| {
            (
                format!("/admin/incidents/{}/delete", incident.id),
                format!("Delete {}?", incident.title),
                incident,
            )
        })
        .collect();
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Incidents", admin: &admin, section: "incidents",
            <div class="page-head">
                <h1>"Incidents"</h1>
                <span class="spacer"></span>
                <a class="btn" href="/admin/incidents/templates">"Templates"</a>
                <a class="btn btn-primary" href="/admin/incidents/new">"Declare incident"</a>
            </div>
            if !scheduled.is_empty() {
                <div class="card table-wrap">
                    <h2>"Scheduled"</h2>
                    <p class="small muted">"Declarations waiting to be published. They are not on status pages yet."</p>
                    scheduled_list(items: &scheduled, now: now)
                </div>
            }
            if rows.is_empty() {
                <div class="card empty">
                    <p><strong>"No incidents."</strong></p>
                    <p>"Monitors that go down open one automatically; declare one to tell visitors what is going on."</p>
                </div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Incident"</th><th>"Status"</th><th>"Impact"</th><th>"Started"</th><th class="hide-sm">"Duration"</th><th class="hide-sm">"Shown"</th><th>"Actions"</th></tr></thead>
                        <tbody>
                            for (delete_url, delete_heading, incident) in &rows {
                                <tr>
                                    <td class="name-cell"><a href=(format!("/admin/incidents/{}", incident.id))>(incident.title.as_str())</a></td>
                                    <td><span class=(status_pill(incident.status))>(incident.status.label())</span></td>
                                    <td><span class=(impact_pill(incident.impact))>(incident.impact.label())</span></td>
                                    <td class="small when">ago(at: incident.started_at, now: now)</td>
                                    <td class="small hide-sm">(fmt::duration(incident.resolved_at.unwrap_or(now).duration_since(incident.started_at).unsigned_abs()))</td>
                                    <td class="small muted hide-sm">"status pages"</td>
                                    <td class="row-actions">
                                        delete_button(action: delete_url,
                                            heading: delete_heading,
                                            body: "It disappears from status pages and the incident history.")
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
        )
    })
}

/// Scheduled declarations or updates: when, what, and edit/cancel actions.
#[component]
async fn scheduled_list(items: &[ScheduledUpdate], now: Timestamp) -> Result<impl View> {
    Ok(view! {
        <ol class="timeline scheduled">
            for item in items {
                <li>
                    <span class=(status_pill(item.status))>(item.status.label())</span>
                    " "
                    <span class="small muted">"Scheduled for " (fmt::when(item.publish_at)) " · " ago(at: item.publish_at, now: now)</span>
                    if let ScheduledKind::Declare { title, .. } = &item.kind {
                        <p><strong>(title.as_str())</strong></p>
                    }
                    <p>(item.body.as_str())</p>
                    if let Some(error) = &item.error {
                        <p class="flash flash-error">"Could not publish: " (error.as_str()) ". Edit it to try again."</p>
                    }
                    <div class="row">
                        <a class="btn btn-link" href=(format!("/admin/incidents/scheduled/{}", item.id))>"Edit"</a>
                        <form method="post" action=(format!("/admin/incidents/scheduled/{}/cancel", item.id))>
                            <button class="btn btn-link" type="submit">"Cancel"</button>
                        </form>
                    </div>
                </li>
            }
        </ol>
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct IncidentForm {
    title: String,
    impact: String,
    status: String,
    message: String,
    /// `datetime-local` (UTC); blank publishes now.
    publish_at: String,
    #[serde(flatten)]
    extra: BTreeMap<String, String>,
}

/// A `publish_at` field: `None` to publish now (blank, or not in the future).
fn publish_at(text: &str, now: Timestamp, errors: &mut FormErrors) -> Option<Timestamp> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match fmt::parse_datetime_local(text) {
        Some(at) if at > now => Some(at),
        Some(_) => None,
        None => {
            errors.insert(
                "publish_at",
                "Enter a date and time (UTC), or leave it blank to publish now.",
            );
            None
        }
    }
}

impl IncidentForm {
    /// The incident, and when to publish it (`None`: now).
    fn to_new(&self, now: Timestamp) -> Result<(NewIncident, Option<Timestamp>), FormErrors> {
        let mut errors = FormErrors::default();
        let publish_at = publish_at(&self.publish_at, now, &mut errors);
        if self.title.trim().is_empty() {
            errors.insert("title", "Give the incident a title.");
        }
        if self.message.trim().is_empty() {
            errors.insert("message", "Write a first update for visitors.");
        }
        let impact = self.impact.parse::<Impact>().unwrap_or_else(|e| {
            errors.insert("impact", e);
            Impact::Minor
        });
        let status = self.status.parse::<IncidentStatus>().unwrap_or_else(|e| {
            errors.insert("status", e);
            IncidentStatus::Investigating
        });
        let monitors = chosen_monitors(&self.extra);
        if monitors.is_empty() {
            errors.insert("monitors", "Choose at least one affected monitor.");
        }
        if errors.is_empty() {
            Ok((
                NewIncident {
                    title: self.title.trim().to_owned(),
                    impact,
                    status,
                    message: self.message.trim().to_owned(),
                    monitors,
                },
                publish_at,
            ))
        } else {
            Err(errors)
        }
    }
}

#[component]
pub(super) async fn select_field(
    name: &str,
    label: &str,
    options: Vec<(&'static str, &'static str)>,
    selected: &str,
) -> Result<impl View> {
    let id = format!("field-{name}");
    Ok(view! {
        <div class="field">
            <label for=(id.clone())>(label)</label>
            <select id=(id) name=(name)>
                for (value, text) in &options {
                    <option value=(*value) selected=(selected == *value)>(*text)</option>
                }
            </select>
        </div>
    })
}

pub(super) fn impacts() -> Vec<(&'static str, &'static str)> {
    Impact::ALL
        .iter()
        .map(|i| (i.as_str(), i.label()))
        .collect()
}

pub(super) fn statuses() -> Vec<(&'static str, &'static str)> {
    IncidentStatus::ALL
        .iter()
        .map(|s| (s.as_str(), s.label()))
        .collect()
}

#[component]
async fn incident_form(
    form: &IncidentForm,
    errors: &FormErrors,
    monitors: &[Monitor],
) -> Result<impl View> {
    let chosen = chosen_monitors(&form.extra);
    Ok(view! {
        <form method="post" action="/admin/incidents">
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            <fieldset>
                <legend>"Incident"</legend>
                text_field(name: "title", label: "Title", value: &form.title, errors: errors, placeholder: "Elevated error rates on the API")
                <div class="fields">
                    select_field(name: "impact", label: "Impact", options: impacts(), selected: &form.impact)
                    select_field(name: "status", label: "Status", options: statuses(), selected: &form.status)
                </div>
                text_area(name: "message", label: "First update", value: &form.message, errors: errors,
                    hint: "Shown on every status page that includes an affected monitor. Placeholders such as {monitor} are filled in when it is published.")
                publish_at_field(value: &form.publish_at, errors: errors)
            </fieldset>
            monitor_toggles(monitors: monitors, chosen: &chosen, errors: errors)
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Declare incident"</button>
                <a class="btn btn-link" href="/admin/incidents">"Cancel"</a>
            </div>
        </form>
    })
}

/// "Publish at": when to publish, in UTC; blank publishes now.
#[component]
async fn publish_at_field(value: &str, errors: &FormErrors) -> Result<impl View> {
    let error = errors.get("publish_at").map(str::to_owned);
    Ok(view! {
        <div class="field">
            <label for="field-publish_at">"Publish at (UTC)"</label>
            <input type="datetime-local" id="field-publish_at" name="publish_at" value=(value)
                aria-invalid=(error.is_some().then_some("true"))>
            if let Some(error) = error {
                <span class="error">(error)</span>
            } else {
                <span class="hint">"Optional. Leave blank to publish now; a later time schedules it."</span>
            }
        </div>
    })
}

/// `?template=<id>`: pre-fill a form from an incident template.
#[topcoat::router::query_params]
pub(crate) struct TemplateQuery {
    template: Option<String>,
}

/// The template asked for in the query, if it exists.
async fn chosen_template(cx: &Cx) -> Result<Option<Template>> {
    let Some(id) = topcoat::router::query_params::<TemplateQuery>(cx)
        .ok()
        .and_then(|q| q.template.as_deref()?.trim().parse::<i64>().ok())
    else {
        return Ok(None);
    };
    Ok(app(cx).store.template(id).await?)
}

#[page("/admin/incidents/new")]
pub(crate) async fn new_incident(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let monitors = state.store.list_monitors().await?;
    let templates = state.store.list_templates().await?;
    let template = chosen_template(cx).await?;
    let form = match &template {
        Some(template) => IncidentForm {
            title: template.spec.title.clone(),
            impact: template.spec.impact.as_str().into(),
            status: template.spec.status.as_str().into(),
            message: template.spec.body.clone(),
            ..IncidentForm::default()
        },
        None => IncidentForm {
            impact: Impact::Minor.as_str().into(),
            status: IncidentStatus::Investigating.as_str().into(),
            ..IncidentForm::default()
        },
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: "Declare incident", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Declare incident"</h1></div>
            <div class="card">
                template_picker(action: "/admin/incidents/new", templates: &templates, chosen: template.as_ref().map(|t| t.id))
                incident_form(form: &form, errors: &errors, monitors: &monitors)
            </div>
        )
    })
}

#[route(POST "/admin/incidents")]
pub(crate) async fn create_incident(cx: &Cx, Form(input): Form<IncidentForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let now = Timestamp::now();
    let errors = match input.to_new(now) {
        Ok((new, Some(at))) => {
            let scheduled = NewScheduledUpdate {
                publish_at: at,
                status: new.status,
                body: new.message.clone(),
                kind: ScheduledKind::Declare {
                    title: new.title.clone(),
                    impact: new.impact,
                    monitors: new.monitors.clone(),
                },
                actor: audit::admin(&admin),
            };
            match state.store.schedule_update(&scheduled).await {
                Ok(scheduled) => {
                    record_schedule(state, &admin, "incident.schedule", &scheduled).await?;
                    return see_other("/admin/incidents").into_response(cx);
                }
                Err(StoreError::UnknownMonitors(keys)) => {
                    let mut errors = FormErrors::default();
                    errors.insert("monitors", format!("Unknown monitors: {keys:?}"));
                    errors
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok((new, None)) => {
            match publish::declare(state, &audit::admin(&admin), &new, Via::Direct, now).await? {
                Ok(incident) => {
                    return see_other(format!("/admin/incidents/{}", incident.id))
                        .into_response(cx);
                }
                Err(StoreError::UnknownMonitors(keys)) => {
                    let mut errors = FormErrors::default();
                    errors.insert("monitors", format!("Unknown monitors: {keys:?}"));
                    errors
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(errors) => errors,
    };
    let monitors = state.store.list_monitors().await?;
    let html = view! { cx =>
        admin_shell(title: "Declare incident", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Declare incident"</h1></div>
            <div class="card">incident_form(form: &input, errors: &errors, monitors: &monitors)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

async fn load_incident(cx: &Cx) -> Result<Incident> {
    Ok(app(cx)
        .store
        .incident(path_id(cx)?)
        .await?
        .ok_or_else(not_found)?)
}

/// What an incident's checks say: when it broke, why, and when it came back.
struct Outage<'a> {
    first_failure: Option<Timestamp>,
    /// The check that brought the monitor back, once resolved.
    recovery: Option<&'a StoredCheck>,
    failed: usize,
    regions: Vec<&'a str>,
    /// Error texts, most frequent first.
    causes: Vec<(String, usize)>,
    /// Status codes of failed checks, most frequent first.
    codes: Vec<(u16, usize)>,
}

impl<'a> Outage<'a> {
    fn of(checks: &'a [StoredCheck]) -> Self {
        let failures: Vec<&StoredCheck> =
            checks.iter().filter(|c| c.health == Health::Down).collect();
        let mut regions: Vec<&str> = checks.iter().map(|c| c.region.as_str()).collect();
        regions.sort_unstable();
        regions.dedup();
        Self {
            first_failure: failures.first().map(|c| c.checked_at),
            recovery: checks.last().filter(|c| c.health != Health::Down),
            failed: failures.len(),
            regions,
            causes: tally(
                failures
                    .iter()
                    .map(|c| c.error.clone().unwrap_or_else(|| "check failed".to_owned())),
            ),
            codes: tally(failures.iter().filter_map(|c| c.status_code)),
        }
    }
}

/// Counts of each distinct value, most frequent first (ties in first-seen order).
fn tally<T: PartialEq>(values: impl Iterator<Item = T>) -> Vec<(T, usize)> {
    let mut counts: Vec<(T, usize)> = Vec::new();
    for value in values {
        match counts.iter_mut().find(|(seen, _)| *seen == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts
}

fn check_outcome(check: &StoredCheck) -> String {
    let code = check.status_code.map(|c| format!("HTTP {c}"));
    let latency = check.latency_ms.map(|ms| format!("{ms}ms"));
    let parts: Vec<String> = code.into_iter().chain(latency).collect();
    if parts.is_empty() {
        "no response".to_owned()
    } else {
        parts.join(" in ")
    }
}

/// The incident at a glance: affected monitors, timing and causes.
#[component]
async fn outage_summary(
    incident: &Incident,
    affected: &[Monitor],
    checks: &[StoredCheck],
    now: Timestamp,
) -> Result<impl View> {
    let outage = Outage::of(checks);
    let began = outage.first_failure.unwrap_or(incident.started_at);
    let ended = incident.resolved_at.unwrap_or(now);
    let lasted = fmt::duration(ended.duration_since(began).unsigned_abs());
    let monitors: Vec<(String, String, String)> = affected
        .iter()
        .map(|m| {
            (
                format!("/admin/monitors/{}", m.id),
                m.spec.name.clone(),
                target(&m.spec.check),
            )
        })
        .collect();
    let causes: Vec<String> = outage
        .causes
        .iter()
        .map(|(cause, n)| format!("{cause} ×{n}"))
        .collect();
    let codes = outage
        .codes
        .iter()
        .map(|(code, n)| format!("{code} ×{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let auto = incident.kind == IncidentKind::Auto;
    Ok(view! {
        <div class="card">
            <h2>(if auto { "Outage" } else { "Summary" })</h2>
            <dl class="facts">
                <dt>"Affects"</dt>
                <dd>
                    for (href, name, target) in &monitors {
                        <div><a href=(href)>(name.as_str())</a> " " <span class="small muted">(target.as_str())</span></div>
                    }
                    if monitors.is_empty() { "—" }
                </dd>
                if outage.first_failure.is_some() {
                    <dt>"First failure"</dt><dd>(fmt::when_precise(began))</dd>
                }
                <dt>(if auto { "Confirmed" } else { "Started" })</dt>
                <dd>(fmt::when_precise(incident.started_at))</dd>
                <dt>(if auto { "Recovered" } else { "Resolved" })</dt>
                <dd>
                    if let Some(at) = incident.resolved_at {
                        (fmt::when_precise(at))
                        if let Some(check) = outage.recovery {
                            <span class="small muted">" · " (check_outcome(check)) " from " (check.region.as_str())</span>
                        }
                    } else {
                        <span class="pill pill-down">"Ongoing"</span>
                    }
                </dd>
                <dt>(if auto { "Down for" } else { "Duration" })</dt>
                <dd>(lasted.as_str()) if incident.resolved_at.is_none() { " so far" }</dd>
                if !checks.is_empty() {
                    <dt>"Checks"</dt>
                    <dd>(format!("{} failed", outage.failed)) " · " (outage.regions.join(", "))</dd>
                    <dt>"Cause"</dt>
                    <dd class="small">
                        for cause in &causes { <div>(cause.as_str())</div> }
                    </dd>
                    <dt>"Status codes"</dt>
                    <dd>(if codes.is_empty() { "none (no response)".to_owned() } else { codes.clone() })</dd>
                }
            </dl>
        </div>
    })
}

/// Every check in the outage, with its error and response.
#[component]
async fn incident_checks(checks: &[StoredCheck]) -> Result<impl View> {
    Ok(view! {
        <div class="card table-wrap">
            <h2>"Checks"</h2>
            <p class="small muted">"The failure streak through the recovering check. Visible only here; status pages show the timeline."</p>
            <table class="list">
                <thead><tr><th>"Checked"</th><th>"Result"</th><th>"State"</th><th class="hide-sm">"Code"</th><th class="num">"Latency"</th><th class="hide-sm">"Region"</th><th>"Error"</th></tr></thead>
                <tbody>
                    for check in checks {
                        <tr>
                            <td class="small when">(fmt::when_precise(check.checked_at))</td>
                            <td>
                                health_pill(health: check.health)
                                if check.excused { " " <span class="small muted">"Excused"</span> }
                            </td>
                            <td>state_pill(state: check.state_after)</td>
                            <td class="hide-sm">(check.status_code.map_or("—".into(), |c| c.to_string()))</td>
                            <td class="num">(check.latency_ms.map_or("—".into(), |ms| format!("{ms}ms")))</td>
                            <td class="small muted hide-sm">(check.region.as_str())</td>
                            <td class="small">
                                (check.error.clone().unwrap_or_else(|| "—".into()))
                                if let Some(body) = &check.response_body {
                                    <details><summary>(format!("Response ({} bytes)", body.len()))</summary><pre class="response">(body.as_str())</pre></details>
                                }
                            </td>
                        </tr>
                    }
                </tbody>
            </table>
        </div>
    })
}

/// `?excused=N`: how many checks "Excuse from uptime" just changed.
#[topcoat::router::query_params]
pub(crate) struct ExcusedQuery {
    excused: Option<u64>,
}

fn excused_message(n: u64) -> String {
    match n {
        0 => "No failed checks left to excuse.".to_owned(),
        1 => "Excused 1 failed check. It counts as up on status pages; count it again from the monitor page.".to_owned(),
        n => format!("Excused {n} failed checks. They count as up on status pages; count them again from the monitor page."),
    }
}

#[page("/admin/incidents/{id}")]
pub(crate) async fn incident_detail(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let incident = load_incident(cx).await?;
    let state = app(cx);
    let checks = state.store.incident_checks(&incident).await?;
    let monitors = state.store.list_monitors().await?;
    let affected: Vec<Monitor> = monitors
        .into_iter()
        .filter(|m| incident.monitor_ids.contains(&m.id))
        .collect();
    let base = format!("/admin/incidents/{}", incident.id);
    let delete_url = format!("{base}/delete");
    let update_url = format!("{base}/updates");
    let excuse_url = format!("{base}/excuse");
    let excused = topcoat::router::query_params::<ExcusedQuery>(cx)
        .ok()
        .and_then(|q| q.excused)
        .map(excused_message);
    let next_status = match incident.status {
        IncidentStatus::Investigating => IncidentStatus::Identified,
        IncidentStatus::Identified => IncidentStatus::Monitoring,
        IncidentStatus::Monitoring | IncidentStatus::Resolved => IncidentStatus::Resolved,
    };
    let templates = state.store.list_templates().await?;
    let template = chosen_template(cx).await?;
    let (next_status, draft) = match &template {
        Some(template) => (template.spec.status, template.spec.body.clone()),
        None => (next_status, String::new()),
    };
    let scheduled = state.store.scheduled_for_incident(incident.id).await?;
    // Check diagnostics have their own table.
    let updates: Vec<_> = incident
        .updates
        .iter()
        .filter(|u| u.check.is_none())
        .cloned()
        .collect();
    let now = Timestamp::now();
    let no_errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: &incident.title, admin: &admin, section: "incidents",
            <div class="page-head">
                <h1>(incident.title.as_str())</h1>
                <span class=(status_pill(incident.status))>(incident.status.label())</span>
                <span class=(impact_pill(incident.impact))>(incident.impact.label())</span>
                <span class="spacer"></span>
                if !incident.monitor_ids.is_empty() {
                    <button class="btn" type="button" data-on:click="el.nextElementSibling.show()"
                        title="Its failed checks count as up in uptime figures and the daily bars. Diagnostics are kept.">"Excuse from uptime"</button>
                    <sb-modal heading="Excuse this outage from uptime?">
                        <p>"Its failed checks count as up in uptime figures and the daily bars on status pages. Diagnostics are kept, and the checks can be counted again from the monitor page."</p>
                        <form id="excuse-form" method="post" action=(excuse_url)>
                            <div class="field">
                                <label for="field-reason">"Reason"</label>
                                <input type="text" id="field-reason" name="reason"
                                    placeholder="e.g. the checker's network was down">
                                <span class="hint">"Optional; kept in the audit trail."</span>
                            </div>
                        </form>
                        <button slot="footer" class="btn" type="button" data-sb-close="cancel">"Cancel"</button>
                        <button slot="footer" class="btn btn-primary" type="submit" form="excuse-form">"Excuse from uptime"</button>
                    </sb-modal>
                }
                delete_button(action: &delete_url, heading: "Delete this incident?",
                    body: "It disappears from status pages and the incident history. Uptime is unaffected: excuse the outage first to clear it from the bars.")
            </div>
            if let Some(message) = &excused {
                <p class="flash" role="status">(message.as_str())</p>
            }
            <div class="cols">
                outage_summary(incident: &incident, affected: &affected, checks: &checks, now: now)
                <div class="card">
                    <h2>"Timeline"</h2>
                    if !scheduled.is_empty() {
                        <h3 class="label">"Scheduled"</h3>
                        scheduled_list(items: &scheduled, now: now)
                        <h3 class="label">"Published"</h3>
                    }
                    <ol class="timeline">
                        for update in &updates {
                            <li>
                                <span class=(status_pill(update.status))>(update.status.label())</span>
                                " "
                                <span class="small muted">ago(at: update.created_at, now: now) " · " (fmt::when(update.created_at))</span>
                                <p>(update.body.as_str())</p>
                            </li>
                        }
                    </ol>
                    <p class="small muted">"Shown on status pages with an affected monitor."</p>
                </div>
            </div>
            if !checks.is_empty() {
                incident_checks(checks: &checks)
            }
            <div class="card">
                <h2>"Post an update"</h2>
                template_picker(action: &base, templates: &templates, chosen: template.as_ref().map(|t| t.id))
                <form method="post" action=(update_url)>
                    select_field(name: "status", label: "Status", options: statuses(), selected: next_status.as_str())
                    text_area(name: "message", label: "Message", value: &draft, errors: &no_errors,
                        hint: "What changed, what visitors should expect. Choose Resolved with a later time to schedule the resolution.")
                    publish_at_field(value: "", errors: &no_errors)
                    <div class="actions"><button class="btn btn-primary" type="submit">"Post update"</button></div>
                </form>
            </div>
        )
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct UpdateForm {
    status: String,
    message: String,
    /// `datetime-local` (UTC); blank publishes now.
    publish_at: String,
}

#[route(POST "/admin/incidents/{id}/updates")]
pub(crate) async fn post_incident_update(
    cx: &Cx,
    Form(input): Form<UpdateForm>,
) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    let status = input
        .status
        .parse::<IncidentStatus>()
        .map_err(|_| not_found())?;
    let message = if input.message.trim().is_empty() {
        format!("Status changed to {}.", status.label().to_lowercase())
    } else {
        input.message
    };
    let now = Timestamp::now();
    let mut errors = FormErrors::default();
    if let Some(at) = publish_at(&input.publish_at, now, &mut errors) {
        let scheduled = match state
            .store
            .schedule_update(&NewScheduledUpdate {
                publish_at: at,
                status,
                body: message,
                kind: ScheduledKind::Update { incident_id: id },
                actor: audit::admin(&admin),
            })
            .await
        {
            Ok(scheduled) => scheduled,
            Err(StoreError::IncidentNotFound(_)) => return Err(not_found().into()),
            Err(error) => return Err(error.into()),
        };
        record_schedule(state, &admin, "incident.schedule", &scheduled).await?;
        return Ok(see_other(format!("/admin/incidents/{id}")));
    }
    if !errors.is_empty() {
        // Browsers always send a valid time; never publish a garbled one now.
        return Ok(see_other(format!("/admin/incidents/{id}")));
    }
    match publish::update(
        state,
        &audit::admin(&admin),
        id,
        status,
        &message,
        Via::Direct,
        now,
    )
    .await?
    {
        Ok(_) => {}
        Err(StoreError::IncidentNotFound(_)) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    }
    Ok(see_other(format!("/admin/incidents/{id}")))
}

/// Stops the incident's failed checks counting against uptime, for each
/// affected monitor, from the first failure until it resolved (or now).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ExcuseForm {
    /// Why, for the audit trail.
    reason: String,
}

#[route(POST "/admin/incidents/{id}/excuse")]
pub(crate) async fn excuse_incident(cx: &Cx, Form(input): Form<ExcuseForm>) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let incident = load_incident(cx).await?;
    let state = app(cx);
    let checks = state.store.incident_checks(&incident).await?;
    let now = Timestamp::now();
    let from = checks.first().map_or(incident.started_at, |c| {
        c.checked_at.min(incident.started_at)
    });
    let to = incident.resolved_at.unwrap_or(now);
    let mut excused = 0;
    for monitor in &incident.monitor_ids {
        excused += state.store.excuse_checks(*monitor, from, to, now).await?;
    }
    state.pages.clear();
    tracing::info!(incident = incident.id, excused, by = %admin.login, "outage excused from uptime");
    let reason = input.reason.trim();
    audit::record(
        state,
        audit::admin(&admin),
        "uptime.excuse",
        audit::target("incident", incident.id),
        format!("Excused {excused} checks of {} from uptime", incident.title),
        json!({
            "from": from,
            "to": to,
            "checks": excused,
            "monitors": incident.monitor_ids.iter().map(|m| m.0).collect::<Vec<_>>(),
            "reason": (!reason.is_empty()).then_some(reason),
        }),
    )
    .await?;
    Ok(see_other(format!(
        "/admin/incidents/{}?excused={excused}",
        incident.id
    )))
}

#[route(POST "/admin/incidents/{id}/delete")]
pub(crate) async fn delete_incident(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    let incident = state.store.incident(id).await?.ok_or_else(not_found)?;
    if !state.store.delete_incident(id).await? {
        return Err(not_found().into());
    }
    state.pages.clear();
    tracing::info!(incident = id, by = %admin.login, "incident deleted");
    audit::record(
        state,
        audit::admin(&admin),
        "incident.delete",
        audit::target("incident", id),
        format!("Deleted incident {}", incident.title),
        json!({
            "title": incident.title,
            "kind": incident.kind.as_str(),
            "started_at": incident.started_at,
            "resolved_at": incident.resolved_at,
        }),
    )
    .await?;
    Ok(see_other("/admin/incidents"))
}

// ── Scheduled declarations and updates ───────────────────────────────────

/// Records scheduling, rescheduling or cancelling `scheduled`.
async fn record_schedule(
    state: &crate::AppState,
    admin: &AdminUser,
    action: &str,
    scheduled: &ScheduledUpdate,
) -> Result<()> {
    let (verb, what) = match action {
        "incident.schedule" => ("Scheduled", "for"),
        "incident.reschedule" => ("Rescheduled", "for"),
        _ => ("Cancelled", "planned for"),
    };
    let subject = match &scheduled.kind {
        ScheduledKind::Declare { title, .. } => format!("the incident {title}"),
        ScheduledKind::Update { incident_id } => {
            format!(
                "a {} update to incident {incident_id}",
                scheduled.status.label().to_lowercase()
            )
        }
    };
    audit::record(
        state,
        audit::admin(admin),
        action,
        scheduled
            .incident_id()
            .and_then(|id| audit::target("incident", id)),
        format!(
            "{verb} {subject} {what} {}",
            fmt::when(scheduled.publish_at)
        ),
        json!({
            "scheduled": scheduled.id,
            "publish_at": scheduled.publish_at,
            "status": scheduled.status.as_str(),
            "message": scheduled.body,
        }),
    )
    .await
}

/// Where a scheduled item is listed: its incident, or the incidents page.
fn scheduled_home(scheduled: &ScheduledUpdate) -> String {
    scheduled.incident_id().map_or_else(
        || "/admin/incidents".to_owned(),
        |id| format!("/admin/incidents/{id}"),
    )
}

async fn load_scheduled(cx: &Cx) -> Result<ScheduledUpdate> {
    Ok(app(cx)
        .store
        .scheduled(path_id(cx)?)
        .await?
        .ok_or_else(not_found)?)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ScheduledForm {
    /// Declarations only.
    title: String,
    impact: String,
    status: String,
    message: String,
    publish_at: String,
}

#[component]
async fn scheduled_form(
    scheduled: &ScheduledUpdate,
    form: &ScheduledForm,
    errors: &FormErrors,
) -> Result<impl View> {
    let declaration = matches!(scheduled.kind, ScheduledKind::Declare { .. });
    Ok(view! {
        <form method="post" action=(format!("/admin/incidents/scheduled/{}", scheduled.id))>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            if declaration {
                text_field(name: "title", label: "Title", value: &form.title, errors: errors)
                select_field(name: "impact", label: "Impact", options: impacts(), selected: &form.impact)
            }
            select_field(name: "status", label: "Status", options: statuses(), selected: &form.status)
            text_area(name: "message", label: "Message", value: &form.message, errors: errors)
            <div class="field">
                <label for="field-publish_at">"Publish at (UTC)"</label>
                <input type="datetime-local" id="field-publish_at" name="publish_at" value=(form.publish_at.as_str()) required=""
                    aria-invalid=(errors.get("publish_at").is_some().then_some("true"))>
                if let Some(error) = errors.get("publish_at") {
                    <span class="error">(error.to_owned())</span>
                }
            </div>
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save"</button>
                <a class="btn btn-link" href=(scheduled_home(scheduled))>"Back"</a>
            </div>
        </form>
    })
}

#[page("/admin/incidents/scheduled/{id}")]
pub(crate) async fn edit_scheduled(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let scheduled = load_scheduled(cx).await?;
    let (title, impact) = match &scheduled.kind {
        ScheduledKind::Declare { title, impact, .. } => (title.clone(), impact.as_str().to_owned()),
        ScheduledKind::Update { .. } => (String::new(), String::new()),
    };
    let form = ScheduledForm {
        title,
        impact,
        status: scheduled.status.as_str().to_owned(),
        message: scheduled.body.clone(),
        publish_at: fmt::datetime_local(scheduled.publish_at),
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: "Scheduled update", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Scheduled update"</h1></div>
            if let Some(error) = &scheduled.error {
                <p class="flash flash-error">"Could not publish: " (error.as_str()) ". Saving tries again."</p>
            }
            <div class="card">scheduled_form(scheduled: &scheduled, form: &form, errors: &errors)</div>
        )
    })
}

#[route(POST "/admin/incidents/scheduled/{id}")]
pub(crate) async fn update_scheduled(
    cx: &Cx,
    Form(input): Form<ScheduledForm>,
) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let scheduled = load_scheduled(cx).await?;
    let mut errors = FormErrors::default();
    let publish_at = match fmt::parse_datetime_local(&input.publish_at) {
        Some(at) => Some(at),
        None => {
            errors.insert("publish_at", "Enter a date and time (UTC).");
            None
        }
    };
    let status = input.status.parse::<IncidentStatus>().unwrap_or_else(|e| {
        errors.insert("status", e);
        IncidentStatus::Investigating
    });
    if input.message.trim().is_empty() {
        errors.insert("message", "Write the message visitors will read.");
    }
    let declaration = match scheduled.kind {
        ScheduledKind::Declare { .. } => {
            if input.title.trim().is_empty() {
                errors.insert("title", "Give the incident a title.");
            }
            let impact = input.impact.parse::<Impact>().unwrap_or_else(|e| {
                errors.insert("impact", e);
                Impact::Minor
            });
            Some((input.title.trim(), impact))
        }
        ScheduledKind::Update { .. } => None,
    };
    if let (true, Some(at)) = (errors.is_empty(), publish_at) {
        let updated = state
            .store
            .reschedule(scheduled.id, at, status, &input.message, declaration)
            .await?;
        record_schedule(state, &admin, "incident.reschedule", &updated).await?;
        return see_other(scheduled_home(&updated)).into_response(cx);
    }
    let html = view! { cx =>
        admin_shell(title: "Scheduled update", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Scheduled update"</h1></div>
            <div class="card">scheduled_form(scheduled: &scheduled, form: &input, errors: &errors)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

#[route(POST "/admin/incidents/scheduled/{id}/cancel")]
pub(crate) async fn cancel_scheduled(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let scheduled = load_scheduled(cx).await?;
    if state.store.cancel_scheduled(scheduled.id).await? {
        record_schedule(state, &admin, "incident.unschedule", &scheduled).await?;
    }
    Ok(see_other(scheduled_home(&scheduled)))
}

// ── Maintenance ──────────────────────────────────────────────────────────

#[page("/admin/maintenance")]
pub(crate) async fn maintenance_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let windows = app(cx).store.list_maintenances().await?;
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Maintenance", admin: &admin, section: "maintenance",
            <div class="page-head">
                <h1>"Maintenance"</h1>
                <span class="spacer"></span>
                <a class="btn btn-primary" href="/admin/maintenance/new">"Schedule maintenance"</a>
            </div>
            if windows.is_empty() {
                <div class="card empty">
                    <p><strong>"Nothing scheduled."</strong></p>
                    <p>"During a window, affected monitors report MAINTENANCE and send no alerts."</p>
                </div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Window"</th><th>"When (UTC)"</th><th class="hide-sm">"Monitors"</th><th></th></tr></thead>
                        <tbody>
                            for window in &windows {
                                <tr>
                                    <td class="name-cell"><a href=(format!("/admin/maintenance/{}", window.id))>(window.spec.title.as_str())</a></td>
                                    <td class="small">
                                        (fmt::when(window.spec.starts_at)) " – " (fmt::when(window.spec.ends_at))
                                        if let Some(repeat) = window.spec.repeat {
                                            <br><span class="muted">"repeats " (repeat.as_str())</span>
                                        }
                                    </td>
                                    <td class="mono small hide-sm">(window.spec.monitors.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))</td>
                                    <td>
                                        if window.spec.is_active(now) {
                                            <span class="pill pill-maintenance">"In progress"</span>
                                        } else if window.spec.repeat.is_none() && window.spec.ends_at <= now {
                                            <span class="pill pill-paused">"Done"</span>
                                        } else {
                                            <span class="pill pill-unknown">"Scheduled"</span>
                                        }
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
        )
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct MaintenanceForm {
    title: String,
    description: String,
    starts_at: String,
    ends_at: String,
    repeat: String,
    repeat_until: String,
    #[serde(flatten)]
    extra: BTreeMap<String, String>,
}

impl MaintenanceForm {
    fn from_spec(spec: &MaintenanceSpec) -> Self {
        Self {
            title: spec.title.clone(),
            description: spec.description.clone().unwrap_or_default(),
            starts_at: fmt::datetime_local(spec.starts_at),
            ends_at: fmt::datetime_local(spec.ends_at),
            repeat: spec
                .repeat
                .map(|r| r.as_str().to_owned())
                .unwrap_or_default(),
            repeat_until: spec
                .repeat_until
                .map(fmt::datetime_local)
                .unwrap_or_default(),
            extra: spec
                .monitors
                .iter()
                .map(|key| (format!("monitor_{key}"), "on".to_owned()))
                .collect(),
        }
    }

    fn to_spec(&self) -> Result<MaintenanceSpec, FormErrors> {
        let mut errors = FormErrors::default();
        let mut time = |field: &'static str, text: &str| {
            fmt::parse_datetime_local(text).unwrap_or_else(|| {
                errors.insert(field, "Use a date and time, e.g. 2031-03-01T22:00 (UTC).");
                Timestamp::UNIX_EPOCH
            })
        };
        let starts_at = time("starts_at", &self.starts_at);
        let ends_at = time("ends_at", &self.ends_at);
        let repeat_until = match self.repeat_until.trim() {
            "" => None,
            text => Some(time("repeat_until", text)),
        };
        let repeat = match self.repeat.trim() {
            "" => None,
            text => text
                .parse::<Repeat>()
                .map_err(|_| errors.insert("repeat", "Choose daily, weekly or no repeat."))
                .ok(),
        };
        let description = Some(self.description.trim().to_owned()).filter(|d| !d.is_empty());
        let spec = MaintenanceSpec {
            title: self.title.trim().to_owned(),
            description,
            starts_at,
            ends_at,
            repeat,
            repeat_until,
            monitors: chosen_monitors(&self.extra),
        };
        if errors.is_empty()
            && let Err(error) = spec.validate()
        {
            errors.insert(error.field(), error.to_string());
        }
        if spec.title.is_empty() {
            errors.insert("title", "Give the maintenance a title.");
        }
        if errors.is_empty() {
            Ok(spec)
        } else {
            Err(errors)
        }
    }
}

#[component]
async fn maintenance_form(
    form: &MaintenanceForm,
    errors: &FormErrors,
    action: &str,
    monitors: &[Monitor],
) -> Result<impl View> {
    let chosen = chosen_monitors(&form.extra);
    Ok(view! {
        <form method="post" action=(action)>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            <fieldset>
                <legend>"Window"</legend>
                text_field(name: "title", label: "Title", value: &form.title, errors: errors, placeholder: "Database upgrade")
                text_area(name: "description", label: "Description", value: &form.description, errors: errors,
                    hint: "Optional; shown on status pages.")
                <div class="fields">
                    <div class="field">
                        <label for="field-starts_at">"Starts (UTC)"</label>
                        <input type="datetime-local" id="field-starts_at" name="starts_at" value=(form.starts_at.as_str())
                            aria-invalid=(errors.get("starts_at").is_some().then_some("true"))>
                        if let Some(error) = errors.get("starts_at") { <span class="error">(error.to_owned())</span> }
                    </div>
                    <div class="field">
                        <label for="field-ends_at">"Ends (UTC)"</label>
                        <input type="datetime-local" id="field-ends_at" name="ends_at" value=(form.ends_at.as_str())
                            aria-invalid=(errors.get("ends_at").is_some().then_some("true"))>
                        if let Some(error) = errors.get("ends_at") { <span class="error">(error.to_owned())</span> }
                    </div>
                </div>
                <div class="fields">
                    <div class="field">
                        <label for="field-repeat">"Repeat"</label>
                        <select id="field-repeat" name="repeat">
                            <option value="" selected=(form.repeat.is_empty())>"Once"</option>
                            for choice in Repeat::ALL {
                                <option value=(choice.as_str()) selected=(form.repeat == choice.as_str())>(choice.label())</option>
                            }
                        </select>
                        if let Some(error) = errors.get("repeat") { <span class="error">(error.to_owned())</span> }
                    </div>
                    <div class="field">
                        <label for="field-repeat_until">"Repeat until (UTC)"</label>
                        <input type="datetime-local" id="field-repeat_until" name="repeat_until" value=(form.repeat_until.as_str())
                            aria-invalid=(errors.get("repeat_until").is_some().then_some("true"))>
                        <span class="hint">"Optional; the last start. Blank repeats forever."</span>
                        if let Some(error) = errors.get("repeat_until") { <span class="error">(error.to_owned())</span> }
                    </div>
                </div>
            </fieldset>
            monitor_toggles(monitors: monitors, chosen: &chosen, errors: errors)
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save window"</button>
                <a class="btn btn-link" href="/admin/maintenance">"Cancel"</a>
            </div>
        </form>
    })
}

async fn maintenance_form_page(
    cx: &Cx,
    admin: &AdminUser,
    title: &str,
    form: &MaintenanceForm,
    errors: &FormErrors,
    action: &str,
) -> Result<Response> {
    let monitors = app(cx).store.list_monitors().await?;
    let html = view! { cx =>
        admin_shell(title: title, admin: admin, section: "maintenance",
            <div class="page-head"><h1>(title)</h1></div>
            <div class="card">maintenance_form(form: form, errors: errors, action: action, monitors: &monitors)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

#[page("/admin/maintenance/new")]
pub(crate) async fn new_maintenance(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let monitors = app(cx).store.list_monitors().await?;
    let errors = FormErrors::default();
    let form = MaintenanceForm::default();
    Ok(view! {
        admin_shell(title: "Schedule maintenance", admin: &admin, section: "maintenance",
            <div class="page-head"><h1>"Schedule maintenance"</h1></div>
            <div class="card">maintenance_form(form: &form, errors: &errors, action: "/admin/maintenance", monitors: &monitors)</div>
        )
    })
}

fn unknown_monitors(keys: &[MonitorKey]) -> FormErrors {
    let mut errors = FormErrors::default();
    let keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
    errors.insert("monitors", format!("Unknown monitors: {}", keys.join(", ")));
    errors
}

#[route(POST "/admin/maintenance")]
pub(crate) async fn create_maintenance(
    cx: &Cx,
    Form(input): Form<MaintenanceForm>,
) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let errors = match input.to_spec() {
        Ok(spec) => match state
            .store
            .create_maintenance(&spec, Timestamp::now())
            .await
        {
            Ok(window) => {
                state.pages.clear();
                tracing::info!(maintenance = window.id, by = %admin.login, "maintenance scheduled");
                audit::record(
                    state,
                    audit::admin(&admin),
                    "maintenance.create",
                    audit::target("maintenance", window.id),
                    format!("Scheduled maintenance {}", spec.title),
                    json!({ "maintenance": audit::value(&spec) }),
                )
                .await?;
                return see_other("/admin/maintenance").into_response(cx);
            }
            Err(StoreError::UnknownMonitors(keys)) => unknown_monitors(&keys),
            Err(error) => return Err(error.into()),
        },
        Err(errors) => errors,
    };
    maintenance_form_page(
        cx,
        &admin,
        "Schedule maintenance",
        &input,
        &errors,
        "/admin/maintenance",
    )
    .await
}

#[page("/admin/maintenance/{id}")]
pub(crate) async fn edit_maintenance(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let store = &app(cx).store;
    let window = store
        .maintenance(path_id(cx)?)
        .await?
        .ok_or_else(not_found)?;
    let monitors = store.list_monitors().await?;
    let form = MaintenanceForm::from_spec(&window.spec);
    let errors = FormErrors::default();
    let action = format!("/admin/maintenance/{}", window.id);
    let delete_url = format!("{action}/delete");
    Ok(view! {
        admin_shell(title: &window.spec.title, admin: &admin, section: "maintenance",
            <div class="page-head">
                <h1>(window.spec.title.as_str())</h1>
                <span class="spacer"></span>
                delete_button(action: &delete_url, heading: "Cancel this maintenance?",
                    body: "The window is deleted; affected monitors alert normally again.")
            </div>
            <div class="card">maintenance_form(form: &form, errors: &errors, action: &action, monitors: &monitors)</div>
        )
    })
}

#[route(POST "/admin/maintenance/{id}")]
pub(crate) async fn update_maintenance(
    cx: &Cx,
    Form(input): Form<MaintenanceForm>,
) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    let action = format!("/admin/maintenance/{id}");
    let before = state
        .store
        .maintenance(id)
        .await?
        .ok_or_else(not_found)?
        .spec;
    let errors = match input.to_spec() {
        Ok(spec) => match state.store.update_maintenance(id, &spec).await {
            Ok(_) => {
                state.pages.clear();
                tracing::info!(maintenance = id, by = %admin.login, "maintenance updated");
                let details = audit::plain_changes(&before, &spec);
                audit::record(
                    state,
                    audit::admin(&admin),
                    "maintenance.update",
                    audit::target("maintenance", id),
                    format!(
                        "Updated maintenance {} ({})",
                        spec.title,
                        audit::changed_fields(&details).join(", ")
                    ),
                    details,
                )
                .await?;
                return see_other("/admin/maintenance").into_response(cx);
            }
            Err(StoreError::MaintenanceNotFound(_)) => return Err(not_found().into()),
            Err(StoreError::UnknownMonitors(keys)) => unknown_monitors(&keys),
            Err(error) => return Err(error.into()),
        },
        Err(errors) => errors,
    };
    maintenance_form_page(cx, &admin, "Edit maintenance", &input, &errors, &action).await
}

#[route(POST "/admin/maintenance/{id}/delete")]
pub(crate) async fn delete_maintenance(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    let window = state.store.maintenance(id).await?.ok_or_else(not_found)?;
    if !state.store.delete_maintenance(id).await? {
        return Err(not_found().into());
    }
    state.pages.clear();
    tracing::info!(maintenance = id, by = %admin.login, "maintenance deleted");
    audit::record(
        state,
        audit::admin(&admin),
        "maintenance.delete",
        audit::target("maintenance", id),
        format!("Cancelled maintenance {}", window.spec.title),
        json!({ "maintenance": audit::value(&window.spec) }),
    )
    .await?;
    Ok(see_other("/admin/maintenance"))
}
