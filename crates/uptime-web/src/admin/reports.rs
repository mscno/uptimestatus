//! A status page's monthly uptime reports: who may read them, the uptime
//! target, the months, and links that open private reports.

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::json;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        path_param, query_params,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{Month, SlaTarget};
use uptime_store::{Page, ReportLink, ReportSettings};

use super::{Id, badges::public_base, form::FormErrors, views::text_field};
use crate::{
    AppState, audit,
    auth::{app, require_admin},
    fmt,
    reports::{hash_link_key, months, new_link_key},
    views::{admin_shell, ago},
};

path_param!(link: i64, error = not_found);

/// The most days a link may last.
const MAX_LINK_DAYS: i64 = 3650;

async fn load_page(cx: &Cx) -> Result<Page> {
    Ok(app(cx)
        .store
        .page(*path_param::<Id>(cx)?)
        .await?
        .ok_or_else(not_found)?)
}

/// `?notice=` / `?error=` after an action.
#[query_params]
pub(crate) struct Flash {
    notice: Option<String>,
    error: Option<String>,
}

fn back(page: i64, key: &str, message: &str) -> SeeOther {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair(key, message);
    see_other(format!("/admin/pages/{page}/reports?{}", query.finish()))
}

/// The page; `created` is a new link's URL, shown once.
async fn reports_view(
    cx: &Cx,
    page: &Page,
    notice: Option<String>,
    error: Option<String>,
    created: Option<String>,
) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let settings = state.store.report_settings(page.id).await?;
    let now = Timestamp::now();
    let months = months(&state.store, page, now).await?;
    let links = state.store.report_links(page.id).await?;
    let public = public_base(state, page).await?;
    let base = format!("/admin/pages/{}", page.id);
    let local = format!("/s/{}", page.spec.slug);
    let title = format!("Reports · {}", page.spec.title);
    let rows: Vec<(String, String, String, bool)> = months
        .iter()
        .map(|month| {
            (
                month.label(),
                format!("{local}/reports/{month}"),
                format!("{local}/reports/{month}.pdf"),
                month.contains(now),
            )
        })
        .collect();
    let link_rows: Vec<(ReportLink, String, bool)> = links
        .into_iter()
        .map(|link| {
            let revoke = format!("{base}/reports/links/{}/revoke", link.id);
            let expired = link.expires_at.is_some_and(|at| at <= now);
            (link, revoke, expired)
        })
        .collect();
    let month_choices: Vec<(String, String)> = months
        .iter()
        .map(|month| (month.to_string(), month.label()))
        .collect();
    let settings_action = format!("{base}/reports/settings");
    let links_action = format!("{base}/reports/links");
    let no_errors = FormErrors::default();
    let html = view! { cx =>
        admin_shell(title: &title, admin: &admin, section: "pages",
            <div class="page-head">
                <h1>"Reports" <span class="muted">" · " (page.spec.title.as_str())</span></h1>
                <span class="spacer"></span>
                <a class="btn" href=(base.clone())>"Back to the page"</a>
            </div>
            if let Some(notice) = &notice { <p class="flash" role="status">(notice.as_str())</p> }
            if let Some(error) = &error { <p class="flash flash-error" role="alert">(error.as_str())</p> }
            if let Some(url) = &created {
                <div class="flash" role="status">
                    <p><strong>"Copy the new link now. It is not shown again."</strong></p>
                    <div class="row small">
                        <code>(url.as_str())</code>
                        <sb-copy-button value=(url.as_str()) label="Copy report link"></sb-copy-button>
                    </div>
                </div>
            }
            settings_form(settings: &settings, action: &settings_action, public: &public)
            <div class="card table-wrap">
                <h2>"Months"</h2>
                if rows.is_empty() {
                    <p class="muted">"No history yet: reports start with the page's first checks."</p>
                } else {
                    <table class="list">
                        <thead><tr><th>"Month"</th><th>"Report"</th></tr></thead>
                        <tbody>
                            for (label, html, pdf, to_date) in &rows {
                                <tr>
                                    <td>(label.as_str()) if *to_date { <span class="small muted">" (to date)"</span> }</td>
                                    <td class="row-actions">
                                        <a class="btn btn-link" href=(html.as_str())>"View"</a>
                                        <a class="btn btn-link" href=(pdf.as_str())>"PDF"</a>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                }
            </div>
            <div class="card table-wrap">
                <h2>"Report links"</h2>
                <p class="small muted">"Links open reports without signing in, even while reports are private. Anyone with a link can read what it opens."</p>
                if link_rows.is_empty() {
                    <p class="muted">"No links yet."</p>
                } else {
                    <table class="list">
                        <thead><tr><th>"Label"</th><th>"Opens"</th><th>"Expires"</th><th class="hide-sm">"Created"</th><th>"Actions"</th></tr></thead>
                        <tbody>
                            for (link, revoke, expired) in &link_rows {
                                <tr>
                                    <td>(link.label.clone().unwrap_or_else(|| "—".into()))</td>
                                    <td>(link.month.map_or_else(|| "Every month".to_owned(), Month::label))</td>
                                    <td class="small">
                                        (link.expires_at.map_or_else(|| "Never".to_owned(), fmt::when_dated))
                                        if *expired { " " <span class="pill pill-down">"Expired"</span> }
                                    </td>
                                    <td class="small muted hide-sm">ago(at: link.created_at, now: now) " by " (link.created_by.as_str())</td>
                                    <td class="row-actions">
                                        <form method="post" action=(revoke.as_str())>
                                            <button class="btn btn-link" type="submit">"Revoke"</button>
                                        </form>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                }
                <form method="post" action=(links_action.as_str())>
                    <h3 class="label">"New link"</h3>
                    <div class="fields">
                        <div class="field">
                            <label for="field-month">"Opens"</label>
                            <select id="field-month" name="month">
                                <option value="">"Every month"</option>
                                for (value, label) in &month_choices {
                                    <option value=(value.as_str())>(label.as_str())</option>
                                }
                            </select>
                        </div>
                        text_field(name: "label", label: "Label", value: "", errors: &no_errors,
                            placeholder: "Acme Corp", hint: "Who it is for; only shown here.")
                        text_field(name: "expires_in_days", label: "Expires after (days)", value: "", errors: &no_errors,
                            hint: "Blank never expires.")
                    </div>
                    <div class="actions"><button class="btn btn-primary" type="submit">"Create link"</button></div>
                </form>
            </div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::OK, Html(html)).into_response(cx)
}

#[component]
async fn settings_form(settings: &ReportSettings, action: &str, public: &str) -> Result<impl View> {
    let target = settings
        .sla_target
        .map(|t| t.to_string().trim_end_matches('%').to_owned())
        .unwrap_or_default();
    let no_errors = FormErrors::default();
    Ok(view! {
        <form class="card" method="post" action=(action.to_owned())>
            <h2>"Settings"</h2>
            <fieldset>
                <legend>"Who can read reports"</legend>
                <label class="check">
                    <input type="radio" name="visibility" value="public" checked=(!settings.private)>
                    " Everyone, from a Reports tab on the published page"
                </label>
                <label class="check">
                    <input type="radio" name="visibility" value="private" checked=(settings.private)>
                    " Only admins and report links"
                </label>
                <p class="small muted">"Public reports are at " <code>(format!("{public}/reports"))</code> "."</p>
            </fieldset>
            text_field(name: "sla_target", label: "Uptime target (%)", value: &target, errors: &no_errors,
                placeholder: "99.9", hint: "Optional. Reports show whether each component met it. Up to three decimals.")
            <div class="actions"><button class="btn btn-primary" type="submit">"Save"</button></div>
        </form>
    })
}

#[route(GET "/admin/pages/{id}/reports")]
pub(crate) async fn reports_page(cx: &Cx) -> Result<Response> {
    let page = load_page(cx).await?;
    let (notice, error) = query_params::<Flash>(cx)
        .ok()
        .map_or((None, None), |f| (f.notice.clone(), f.error.clone()));
    reports_view(cx, &page, notice, error, None).await
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SettingsForm {
    visibility: String,
    sla_target: String,
}

#[route(POST "/admin/pages/{id}/reports/settings")]
pub(crate) async fn save_report_settings(
    cx: &Cx,
    Form(input): Form<SettingsForm>,
) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let target = input.sla_target.trim();
    let sla_target = if target.is_empty() {
        None
    } else {
        match target.parse::<SlaTarget>() {
            Ok(target) => Some(target),
            Err(message) => return Ok(back(page.id, "error", &message)),
        }
    };
    let before = state.store.report_settings(page.id).await?;
    let after = ReportSettings {
        private: input.visibility == "private",
        sla_target,
    };
    state.store.set_report_settings(page.id, &after).await?;
    state.page_changed(page.spec.slug.as_str());
    record(
        state,
        &audit::admin(&admin),
        "report.settings",
        &page,
        format!("Changed the report settings of {}", page.spec.title),
        json!({
            "private": { "from": before.private, "to": after.private },
            "sla_target": {
                "from": before.sla_target.map(|t| t.to_string()),
                "to": after.sla_target.map(|t| t.to_string()),
            },
        }),
    )
    .await?;
    Ok(back(page.id, "notice", "Report settings saved."))
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct LinkForm {
    month: String,
    label: String,
    expires_in_days: String,
}

#[route(POST "/admin/pages/{id}/reports/links")]
pub(crate) async fn create_report_link(cx: &Cx, Form(input): Form<LinkForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let now = Timestamp::now();
    let month = match input.month.trim() {
        "" => None,
        text => match text.parse::<Month>() {
            Ok(month) => Some(month),
            Err(message) => return reports_view(cx, &page, None, Some(message), None).await,
        },
    };
    let expires_at = match input.expires_in_days.trim() {
        "" => None,
        text => match text.parse::<i64>() {
            Ok(days) if (1..=MAX_LINK_DAYS).contains(&days) => {
                now.checked_add(SignedDuration::from_hours(days * 24)).ok()
            }
            _ => {
                let message = format!("Expiry is a number of days from 1 to {MAX_LINK_DAYS}.");
                return reports_view(cx, &page, None, Some(message), None).await;
            }
        },
    };
    let key = new_link_key();
    let link = state
        .store
        .create_report_link(
            page.id,
            month,
            Some(input.label.as_str()),
            expires_at,
            &audit::admin(&admin),
            &hash_link_key(&key),
        )
        .await?;
    let opens = month.map_or_else(|| "every month".to_owned(), Month::label);
    record(
        state,
        &audit::admin(&admin),
        "report.link_create",
        &page,
        format!("Created a report link to {opens} of {}", page.spec.title),
        json!({
            "link": link.id,
            "month": month.map(|m| m.to_string()),
            "label": link.label,
            "expires_at": link.expires_at,
        }),
    )
    .await?;
    let public = public_base(state, &page).await?;
    let url = match month {
        Some(month) => format!("{public}/reports/{month}?key={key}"),
        None => format!("{public}/reports?key={key}"),
    };
    reports_view(cx, &page, None, None, Some(url)).await
}

#[route(POST "/admin/pages/{id}/reports/links/{link}/revoke")]
pub(crate) async fn revoke_report_link(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let id = *path_param::<Link>(cx)?;
    if state.store.delete_report_link(page.id, id).await? {
        record(
            state,
            &audit::admin(&admin),
            "report.link_revoke",
            &page,
            format!("Revoked a report link of {}", page.spec.title),
            json!({ "link": id }),
        )
        .await?;
    }
    Ok(back(page.id, "notice", "Link revoked."))
}

async fn record(
    state: &AppState,
    actor: &str,
    action: &str,
    page: &Page,
    summary: String,
    details: serde_json::Value,
) -> Result<()> {
    audit::record(
        state,
        actor,
        action,
        audit::target("page", page.id),
        summary,
        details,
    )
    .await
}
