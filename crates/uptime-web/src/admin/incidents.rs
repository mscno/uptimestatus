//! Incidents (declare, post updates, resolve) and maintenance windows.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::Deserialize;
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
use uptime_domain::{Impact, IncidentKind, IncidentStatus, MaintenanceSpec, MonitorKey, Repeat};
use uptime_store::{AdminUser, Incident, Monitor, NewIncident, StoreError};

use super::{
    Id,
    form::FormErrors,
    views::{checkbox, text_area, text_field},
};
use crate::{
    auth::{app, require_admin},
    fmt,
    views::{admin_shell, ago, delete_button},
};

fn path_id(cx: &Cx) -> Result<i64> {
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

fn impact_pill(impact: Impact) -> &'static str {
    match impact {
        Impact::None => "pill pill-maintenance",
        Impact::Minor => "pill pill-degraded",
        Impact::Major | Impact::Critical => "pill pill-down",
    }
}

fn status_pill(status: IncidentStatus) -> &'static str {
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
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Incidents", admin: &admin, section: "incidents",
            <div class="page-head">
                <h1>"Incidents"</h1>
                <span class="spacer"></span>
                <a class="btn btn-primary" href="/admin/incidents/new">"Declare incident"</a>
            </div>
            if incidents.is_empty() {
                <div class="card empty">
                    <p><strong>"No incidents."</strong></p>
                    <p>"Monitors that go down open one automatically; declare one to tell visitors what is going on."</p>
                </div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Incident"</th><th>"Status"</th><th>"Impact"</th><th>"Started"</th><th class="hide-sm">"Duration"</th><th class="hide-sm">"Shown"</th></tr></thead>
                        <tbody>
                            for incident in &incidents {
                                <tr>
                                    <td class="name-cell"><a href=(format!("/admin/incidents/{}", incident.id))>(incident.title.as_str())</a></td>
                                    <td><span class=(status_pill(incident.status))>(incident.status.label())</span></td>
                                    <td><span class=(impact_pill(incident.impact))>(incident.impact.label())</span></td>
                                    <td class="small when">ago(at: incident.started_at, now: now)</td>
                                    <td class="small hide-sm">(fmt::duration(incident.resolved_at.unwrap_or(now).duration_since(incident.started_at).unsigned_abs()))</td>
                                    <td class="small muted hide-sm">"status pages"</td>
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
pub(crate) struct IncidentForm {
    title: String,
    impact: String,
    status: String,
    message: String,
    #[serde(flatten)]
    extra: BTreeMap<String, String>,
}

impl IncidentForm {
    fn to_new(&self) -> Result<NewIncident, FormErrors> {
        let mut errors = FormErrors::default();
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
            Ok(NewIncident {
                title: self.title.trim().to_owned(),
                impact,
                status,
                message: self.message.trim().to_owned(),
                monitors,
            })
        } else {
            Err(errors)
        }
    }
}

#[component]
async fn select_field(
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

fn impacts() -> Vec<(&'static str, &'static str)> {
    Impact::ALL
        .iter()
        .map(|i| (i.as_str(), i.label()))
        .collect()
}

fn statuses() -> Vec<(&'static str, &'static str)> {
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
                    hint: "Shown on every status page that includes an affected monitor.")
            </fieldset>
            monitor_toggles(monitors: monitors, chosen: &chosen, errors: errors)
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Declare incident"</button>
                <a class="btn btn-link" href="/admin/incidents">"Cancel"</a>
            </div>
        </form>
    })
}

#[page("/admin/incidents/new")]
pub(crate) async fn new_incident(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let monitors = app(cx).store.list_monitors().await?;
    let form = IncidentForm {
        impact: Impact::Minor.as_str().into(),
        status: IncidentStatus::Investigating.as_str().into(),
        ..IncidentForm::default()
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: "Declare incident", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Declare incident"</h1></div>
            <div class="card">incident_form(form: &form, errors: &errors, monitors: &monitors)</div>
        )
    })
}

#[route(POST "/admin/incidents")]
pub(crate) async fn create_incident(cx: &Cx, Form(input): Form<IncidentForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let errors = match input.to_new() {
        Ok(new) => match state.store.declare_incident(&new, Timestamp::now()).await {
            Ok(incident) => {
                state.pages.clear();
                tracing::info!(incident = incident.id, by = %admin.login, "incident declared");
                return see_other(format!("/admin/incidents/{}", incident.id)).into_response(cx);
            }
            Err(StoreError::UnknownMonitors(keys)) => {
                let mut errors = FormErrors::default();
                errors.insert("monitors", format!("Unknown monitors: {keys:?}"));
                errors
            }
            Err(error) => return Err(error.into()),
        },
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

#[page("/admin/incidents/{id}")]
pub(crate) async fn incident_detail(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let incident = load_incident(cx).await?;
    let monitors = app(cx).store.list_monitors().await?;
    let affected: Vec<String> = monitors
        .iter()
        .filter(|m| incident.monitor_ids.contains(&m.id))
        .map(|m| m.spec.name.clone())
        .collect();
    let base = format!("/admin/incidents/{}", incident.id);
    let delete_url = format!("{base}/delete");
    let update_url = format!("{base}/updates");
    let next_status = match incident.status {
        IncidentStatus::Investigating => IncidentStatus::Identified,
        IncidentStatus::Identified => IncidentStatus::Monitoring,
        IncidentStatus::Monitoring | IncidentStatus::Resolved => IncidentStatus::Resolved,
    };
    let now = Timestamp::now();
    let no_errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: &incident.title, admin: &admin, section: "incidents",
            <div class="page-head">
                <h1>(incident.title.as_str())</h1>
                <span class=(status_pill(incident.status))>(incident.status.label())</span>
                <span class=(impact_pill(incident.impact))>(incident.impact.label())</span>
                <span class="spacer"></span>
                delete_button(action: &delete_url, heading: "Delete this incident?",
                    body: "It disappears from status pages and the incident history.")
            </div>
            <div class="cols">
                <div class="card">
                    <h2>"Timeline"</h2>
                    <ol class="timeline">
                        for update in &incident.updates {
                            <li>
                                <span class=(status_pill(update.status))>(update.status.label())</span>
                                " "
                                <span class="small muted">ago(at: update.created_at, now: now) " · " (fmt::when(update.created_at))</span>
                                <p>(update.body.as_str())</p>
                                if let Some(check) = &update.check {
                                    <p class="small muted">
                                        "Check: " (check.state_after.as_str()) " · Region: " (check.region.as_str())
                                        if let Some(code) = check.status_code { " · HTTP " (code) }
                                        if let Some(ms) = check.latency_ms { " · " (ms) " ms" }
                                        if let Some(kind) = check.error_kind { " · " (kind.as_str()) }
                                    </p>
                                    if let Some(body) = &check.response_body {
                                        <details><summary>"Response excerpt"</summary><pre>(body.as_str())</pre></details>
                                    }
                                }

                            </li>
                        }
                    </ol>
                    <p class="small muted">
                        (if incident.kind == IncidentKind::Auto { "Opened automatically. Check diagnostics are visible only here." } else { "Shown on status pages with an affected monitor." })
                        " Affects: " (affected.join(", "))
                    </p>
                </div>
                <div class="card">
                    <h2>"Post an update"</h2>
                    <form method="post" action=(update_url)>
                        select_field(name: "status", label: "Status", options: statuses(), selected: next_status.as_str())
                        text_area(name: "message", label: "Message", value: "", errors: &no_errors,
                            hint: "What changed, what visitors should expect.")
                        <div class="actions"><button class="btn btn-primary" type="submit">"Post update"</button></div>
                    </form>
                </div>
            </div>
        )
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct UpdateForm {
    status: String,
    message: String,
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
    match state
        .store
        .post_incident_update(id, status, &message, Timestamp::now())
        .await
    {
        Ok(_) => {}
        Err(StoreError::IncidentNotFound(_)) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    }
    state.pages.clear();
    tracing::info!(incident = id, status = %status, by = %admin.login, "incident updated");
    Ok(see_other(format!("/admin/incidents/{id}")))
}

#[route(POST "/admin/incidents/{id}/delete")]
pub(crate) async fn delete_incident(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    if !state.store.delete_incident(id).await? {
        return Err(not_found().into());
    }
    state.pages.clear();
    tracing::info!(incident = id, by = %admin.login, "incident deleted");
    Ok(see_other("/admin/incidents"))
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
    let errors = match input.to_spec() {
        Ok(spec) => match state.store.update_maintenance(id, &spec).await {
            Ok(_) => {
                state.pages.clear();
                tracing::info!(maintenance = id, by = %admin.login, "maintenance updated");
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
    if !state.store.delete_maintenance(id).await? {
        return Err(not_found().into());
    }
    state.pages.clear();
    tracing::info!(maintenance = id, by = %admin.login, "maintenance deleted");
    Ok(see_other("/admin/maintenance"))
}
