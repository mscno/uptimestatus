//! Incident templates: reusable wording admins pick when declaring or
//! updating an incident.

use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        page,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{Impact, IncidentStatus, IncidentTemplate};
use uptime_store::{AdminUser, StoreError, Template};

use super::{
    form::FormErrors,
    incidents::{impact_pill, impacts, path_id, select_field, status_pill, statuses},
    views::{text_area, text_field},
};
use crate::{
    audit,
    auth::{app, require_admin},
    views::{admin_shell, delete_button},
};

/// Where the placeholders are explained, under every template body.
const PLACEHOLDERS: &str = "Placeholders, filled in when the incident is published: \
    {monitor} (the first affected monitor), {monitors} (all of them), {page} (the status \
    pages showing it) and {started_at} (when the incident started).";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TemplateForm {
    name: String,
    title: String,
    impact: String,
    status: String,
    body: String,
}

impl TemplateForm {
    fn of(spec: &IncidentTemplate) -> Self {
        Self {
            name: spec.name.clone(),
            title: spec.title.clone(),
            impact: spec.impact.as_str().to_owned(),
            status: spec.status.as_str().to_owned(),
            body: spec.body.clone(),
        }
    }

    fn to_spec(&self) -> std::result::Result<IncidentTemplate, FormErrors> {
        let mut errors = FormErrors::default();
        let impact = self.impact.parse::<Impact>().unwrap_or_else(|e| {
            errors.insert("impact", e);
            Impact::Minor
        });
        let status = self.status.parse::<IncidentStatus>().unwrap_or_else(|e| {
            errors.insert("status", e);
            IncidentStatus::Investigating
        });
        let spec = IncidentTemplate {
            name: self.name.clone(),
            title: self.title.clone(),
            impact,
            status,
            body: self.body.clone(),
        };
        match spec.normalized() {
            Ok(spec) if errors.is_empty() => Ok(spec),
            Ok(_) => Err(errors),
            Err(error) => {
                errors.insert(error.field, error.message);
                Err(errors)
            }
        }
    }
}

#[component]
async fn template_form(
    action: &str,
    form: &TemplateForm,
    errors: &FormErrors,
) -> Result<impl View> {
    Ok(view! {
        <form method="post" action=(action.to_owned())>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            text_field(name: "name", label: "Name", value: &form.name, errors: errors,
                placeholder: "Degraded performance", hint: "Shown in the template picker.")
            text_field(name: "title", label: "Incident title", value: &form.title, errors: errors,
                placeholder: "{monitor} is slower than usual")
            <div class="fields">
                select_field(name: "impact", label: "Impact", options: impacts(), selected: &form.impact)
                select_field(name: "status", label: "Status", options: statuses(), selected: &form.status)
            </div>
            text_area(name: "body", label: "Message", value: &form.body, errors: errors, hint: PLACEHOLDERS)
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save template"</button>
                <a class="btn btn-link" href="/admin/incidents/templates">"Cancel"</a>
            </div>
        </form>
    })
}

#[page("/admin/incidents/templates")]
pub(crate) async fn templates_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let templates = app(cx).store.list_templates().await?;
    let rows: Vec<(String, String, Template)> = templates
        .into_iter()
        .map(|t| {
            (
                format!("/admin/incidents/templates/{}", t.id),
                format!("Delete the template {}?", t.spec.name),
                t,
            )
        })
        .collect();
    let deletes: Vec<String> = rows
        .iter()
        .map(|(base, ..)| format!("{base}/delete"))
        .collect();
    Ok(view! {
        admin_shell(title: "Incident templates", admin: &admin, section: "incidents",
            <div class="page-head">
                <h1>"Incident templates"</h1>
                <span class="spacer"></span>
                <a class="btn" href="/admin/incidents">"Incidents"</a>
                <a class="btn btn-primary" href="/admin/incidents/templates/new">"New template"</a>
            </div>
            if rows.is_empty() {
                <div class="card empty">
                    <p><strong>"No templates yet."</strong></p>
                    <p>"Save the wording you use for common incidents, then pick it when declaring or updating one."</p>
                </div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Name"</th><th>"Title"</th><th>"Impact"</th><th class="hide-sm">"Status"</th><th>"Actions"</th></tr></thead>
                        <tbody>
                            for ((base, heading, template), delete) in rows.iter().zip(&deletes) {
                                <tr>
                                    <td class="name-cell"><a href=(format!("{base}/edit"))>(template.spec.name.as_str())</a></td>
                                    <td class="small">(template.spec.title.as_str())</td>
                                    <td><span class=(impact_pill(template.spec.impact))>(template.spec.impact.label())</span></td>
                                    <td class="hide-sm"><span class=(status_pill(template.spec.status))>(template.spec.status.label())</span></td>
                                    <td class="row-actions">
                                        <a class="btn btn-link" href=(format!("/admin/incidents/new?template={}", template.id))>"Declare"</a>
                                        delete_button(action: delete, heading: heading,
                                            body: "Incidents already declared from it are unchanged.")
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

#[page("/admin/incidents/templates/new")]
pub(crate) async fn new_template(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let form = TemplateForm {
        impact: Impact::Minor.as_str().into(),
        status: IncidentStatus::Investigating.as_str().into(),
        ..TemplateForm::default()
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: "New incident template", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"New incident template"</h1></div>
            <div class="card">template_form(action: "/admin/incidents/templates", form: &form, errors: &errors)</div>
        )
    })
}

#[page("/admin/incidents/templates/{id}/edit")]
pub(crate) async fn edit_template(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let template = app(cx)
        .store
        .template(path_id(cx)?)
        .await?
        .ok_or_else(not_found)?;
    let form = TemplateForm::of(&template.spec);
    let action = format!("/admin/incidents/templates/{}", template.id);
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: &template.spec.name, admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Edit " (template.spec.name.as_str())</h1></div>
            <div class="card">template_form(action: &action, form: &form, errors: &errors)</div>
        )
    })
}

#[route(POST "/admin/incidents/templates")]
pub(crate) async fn create_template(cx: &Cx, Form(input): Form<TemplateForm>) -> Result<Response> {
    save(cx, None, input).await
}

#[route(POST "/admin/incidents/templates/{id}")]
pub(crate) async fn update_template(cx: &Cx, Form(input): Form<TemplateForm>) -> Result<Response> {
    let id = path_id(cx)?;
    save(cx, Some(id), input).await
}

async fn save(cx: &Cx, id: Option<i64>, input: TemplateForm) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let errors = match input.to_spec() {
        Ok(spec) => {
            let before = match id {
                Some(id) => Some(state.store.template(id).await?.ok_or_else(not_found)?),
                None => None,
            };
            let saved = match id {
                Some(id) => state.store.update_template(id, &spec).await,
                None => state.store.create_template(&spec).await,
            };
            match saved {
                Ok(template) => {
                    record(state, &admin, before.as_ref(), &template).await?;
                    return see_other("/admin/incidents/templates").into_response(cx);
                }
                Err(StoreError::DuplicateTemplate(name)) => {
                    let mut errors = FormErrors::default();
                    errors.insert("name", format!("A template named “{name}” already exists."));
                    errors
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(errors) => errors,
    };
    let action = id.map_or_else(
        || "/admin/incidents/templates".to_owned(),
        |id| format!("/admin/incidents/templates/{id}"),
    );
    let html = view! { cx =>
        admin_shell(title: "Incident template", admin: &admin, section: "incidents",
            <div class="page-head"><h1>"Incident template"</h1></div>
            <div class="card">template_form(action: &action, form: &input, errors: &errors)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

async fn record(
    state: &crate::AppState,
    admin: &AdminUser,
    before: Option<&Template>,
    after: &Template,
) -> Result<()> {
    let (action, summary, details) = match before {
        None => (
            "template.create",
            format!("Created the incident template {}", after.spec.name),
            audit::value(&after.spec),
        ),
        Some(before) => (
            "template.update",
            format!("Updated the incident template {}", after.spec.name),
            audit::plain_changes(&before.spec, &after.spec),
        ),
    };
    audit::record(
        state,
        audit::admin(admin),
        action,
        audit::target("template", after.id),
        summary,
        details,
    )
    .await
}

#[route(POST "/admin/incidents/templates/{id}/delete")]
pub(crate) async fn delete_template(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = path_id(cx)?;
    let state = app(cx);
    let template = state.store.template(id).await?.ok_or_else(not_found)?;
    state.store.delete_template(id).await?;
    audit::record(
        state,
        audit::admin(&admin),
        "template.delete",
        audit::target("template", id),
        format!("Deleted the incident template {}", template.spec.name),
        audit::value(&template.spec),
    )
    .await?;
    Ok(see_other("/admin/incidents/templates"))
}

/// A GET form that reloads the page with `?template=<id>`, pre-filling the
/// form below it (submits itself on change; the button covers no-JS).
#[component]
pub(crate) async fn template_picker(
    action: &str,
    templates: &[Template],
    chosen: Option<i64>,
) -> Result<impl View> {
    Ok(view! {
        if !templates.is_empty() {
            <form class="row template-picker" method="get" action=(action.to_owned())>
                <div class="field">
                    <label for="field-template">"Start from a template"</label>
                    <select id="field-template" name="template" data-on:change="el.form.submit()">
                        <option value="">"No template"</option>
                        for template in templates {
                            <option value=(template.id.to_string()) selected=(chosen == Some(template.id))>(template.spec.name.as_str())</option>
                        }
                    </select>
                </div>
                <button class="btn" type="submit">"Use"</button>
                <a class="btn btn-link" href="/admin/incidents/templates">"Manage templates"</a>
            </form>
        }
    })
}
