//! Status page editor.

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
use uptime_store::{AdminUser, StoreError};

use super::{
    Id,
    domains::domains_section,
    form::FormErrors,
    page_form::PageForm,
    views::{checkbox, text_area, text_field},
};
use crate::{
    auth::{app, require_admin},
    views::{admin_shell, delete_button},
};

fn page_id(cx: &Cx) -> Result<i64> {
    Ok(*topcoat::router::path_param::<Id>(cx)?)
}

#[page("/admin/pages")]
pub(crate) async fn pages_list(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let pages = app(cx).store.list_pages().await?;
    Ok(view! {
        admin_shell(title: "Status pages", admin: &admin, section: "pages",
            <div class="page-head">
                <h1>"Status pages"</h1>
                <span class="spacer"></span>
                <a class="btn" href="/admin/export.toml" title="Monitors and pages as TOML, for `uptimestatus seed`">"Export TOML"</a>
                <a class="btn btn-primary" href="/admin/pages/new">"New page"</a>
            </div>
            if pages.is_empty() {
                <div class="card empty"><p><strong>"No status pages yet."</strong></p><p>"Create one to share your monitors' status."</p></div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Page"</th><th class="hide-sm">"Public URL"</th><th class="num hide-sm">"Components"</th><th>"Visibility"</th><th></th></tr></thead>
                        <tbody>
                            for page in &pages {
                                <tr>
                                    <td class="name-cell"><a href=(format!("/admin/pages/{}", page.id))>(page.title.as_str())</a></td>
                                    <td class="mono small hide-sm"><a href=(format!("/s/{}", page.slug))>(format!("/s/{}", page.slug))</a></td>
                                    <td class="num hide-sm">(page.components)</td>
                                    <td>(if page.published { "Published" } else { "Draft" })</td>
                                    <td class="row-actions">
                                        <a href=(format!("/admin/pages/{}", page.id))>"Edit"</a>
                                        <a href=(format!("/admin/pages/new?from={}", page.id))>"Duplicate"</a>
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

#[component]
async fn page_editor(
    form: &PageForm,
    errors: &FormErrors,
    action: &str,
    monitor_keys: &[String],
) -> Result<impl View> {
    let themes = [
        ("auto", "Match the visitor's system"),
        ("light", "Light"),
        ("dark", "Dark"),
    ];
    let looks = [
        ("pixel", "8-bit (pixel font and starfield)"),
        ("clean", "Clean (plain fonts, no backdrop)"),
    ];
    Ok(view! {
        <form method="post" action=(action)>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            <fieldset>
                <legend>"Page"</legend>
                <div class="fields">
                    text_field(name: "title", label: "Title", value: &form.title, errors: errors, placeholder: "Platform Status")
                    text_field(name: "slug", label: "Slug", value: &form.slug, errors: errors, hint: "The page lives at /s/<slug>.", placeholder: "platform")
                </div>
                text_field(name: "description", label: "Description", value: &form.description, errors: errors, hint: "Optional line under the title.")
                <div class="fields">
                    text_field(name: "website_url", label: "Website", value: &form.website_url, errors: errors,
                        placeholder: "https://example.com", hint: "Optional. Linked from the page header so visitors can get back.")
                    text_field(name: "website_label", label: "Link text", value: &form.website_label, errors: errors,
                        placeholder: "Back to example.com", hint: "Optional. Blank: \"Back to\" and the website's host.")
                </div>
                <div class="fields">
                    text_field(name: "accent", label: "Accent color", value: &form.accent, errors: errors, hint: "Optional, e.g. #4f46e5.")
                    <div class="field">
                        <label for="field-theme">"Theme"</label>
                        <select id="field-theme" name="theme">
                            for (value, label) in themes {
                                <option value=(value) selected=(form.theme == value)>(label)</option>
                            }
                        </select>
                    </div>
                </div>
                <div class="fields">
                    <div class="field">
                        <label for="field-look">"Look"</label>
                        <select id="field-look" name="look">
                            for (value, label) in looks {
                                <option value=(value) selected=(form.look == value)>(label)</option>
                            }
                        </select>
                    </div>
                </div>
                checkbox(name: "published", label: "Published (visible to everyone)", checked: form.published.is_some())
            </fieldset>
            <fieldset>
                <legend>"Layout"</legend>
                text_area(name: "layout", label: "Sections and monitors", value: &form.layout, errors: errors,
                    hint: "`## Section` starts a section; each line below is a monitor key, optionally `key | Display name`.")
                <p class="small muted">"Monitor keys: "
                    if monitor_keys.is_empty() { "none yet — create monitors first." } else {
                        for key in monitor_keys { <code>(key.as_str())</code> " " }
                    }
                </p>
            </fieldset>
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save page"</button>
                <a class="btn btn-link" href="/admin/pages">"Cancel"</a>
            </div>
        </form>
    })
}

async fn monitor_keys(cx: &Cx) -> Result<Vec<String>> {
    Ok(app(cx)
        .store
        .list_monitors()
        .await?
        .into_iter()
        .map(|m| m.spec.key.to_string())
        .collect())
}

#[page("/admin/pages/new")]
pub(crate) async fn new_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let keys = monitor_keys(cx).await?;
    let (title, form) = match super::copied_from(cx) {
        Some(id) => {
            let store = &app(cx).store;
            let source = store.page(id).await?.ok_or_else(not_found)?;
            let taken: Vec<String> = store
                .list_pages()
                .await?
                .into_iter()
                .map(|p| p.slug.to_string())
                .collect();
            let form = PageForm::duplicate(&source.spec, &taken);
            (format!("Duplicate {}", source.spec.title), form)
        }
        None => ("New status page".to_owned(), PageForm::new_page()),
    };
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: &title, admin: &admin, section: "pages",
            <div class="page-head"><h1>(title.as_str())</h1></div>
            <div class="card">page_editor(form: &form, errors: &errors, action: "/admin/pages", monitor_keys: &keys)</div>
        )
    })
}

#[route(POST "/admin/pages")]
pub(crate) async fn create_page(cx: &Cx, Form(input): Form<PageForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let errors = match input.to_spec() {
        Ok(spec) => match state.store.create_page(&spec).await {
            Ok(page) => {
                state.pages.clear();
                tracing::info!(page = page.id, slug = %spec.slug, by = %admin.login, "status page created");
                return see_other(format!("/admin/pages/{}", page.id)).into_response(cx);
            }
            Err(error) => store_errors(error)?,
        },
        Err(errors) => errors,
    };
    editor_page(
        cx,
        &admin,
        "New status page",
        &input,
        &errors,
        "/admin/pages",
    )
    .await
}

#[page("/admin/pages/{id}")]
pub(crate) async fn edit_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let id = page_id(cx)?;
    let page = app(cx).store.page(id).await?.ok_or_else(not_found)?;
    let keys = monitor_keys(cx).await?;
    let form = PageForm::from_spec(&page.spec);
    let errors = FormErrors::default();
    let action = format!("/admin/pages/{id}");
    let public = format!("/s/{}", page.spec.slug);
    let delete_url = format!("{action}/delete");
    let domains = app(cx).store.domains_for_page(id).await?;
    let edge = app(cx).edge.clone();
    let uploads_enabled = app(cx).media.is_some();
    let image_error = super::images::image_error(cx);
    Ok(view! {
        admin_shell(title: &page.spec.title, admin: &admin, section: "pages",
            <div class="page-head">
                <h1>(page.spec.title.as_str())</h1>
                <span class="spacer"></span>
                <a class="btn" href=(public)>"View page"</a>
                <a class="btn" href=(format!("/admin/pages/new?from={id}"))>"Duplicate"</a>
                delete_button(action: &delete_url, heading: "Delete this status page?",
                    body: "Its public URL and custom domains stop working. Monitors are kept.")
            </div>
            <div class="card">page_editor(form: &form, errors: &errors, action: &action, monitor_keys: &keys)</div>
            super::images::images_section(page_id: id, logo: page.logo.clone(), favicon: page.favicon.clone(),
                enabled: uploads_enabled, error: image_error)
            domains_section(page_id: id, domains: &domains, edge: &edge)
        )
    })
}

#[route(POST "/admin/pages/{id}")]
pub(crate) async fn update_page(cx: &Cx, Form(input): Form<PageForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let id = page_id(cx)?;
    let state = app(cx);
    let action = format!("/admin/pages/{id}");
    let errors = match input.to_spec() {
        Ok(spec) => match state.store.update_page(id, &spec).await {
            Ok(_) => {
                state.pages.clear();
                tracing::info!(page = id, by = %admin.login, "status page updated");
                return see_other(&action).into_response(cx);
            }
            Err(error) => store_errors(error)?,
        },
        Err(errors) => errors,
    };
    editor_page(cx, &admin, "Edit status page", &input, &errors, &action).await
}

#[route(POST "/admin/pages/{id}/delete")]
pub(crate) async fn delete_page(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = page_id(cx)?;
    let state = app(cx);
    if !state.store.delete_page(id).await? {
        return Err(not_found().into());
    }
    state.pages.clear();
    tracing::info!(page = id, by = %admin.login, "status page deleted");
    Ok(see_other("/admin/pages"))
}

/// Store rejections that belong on the form; anything else is an error.
fn store_errors(error: StoreError) -> Result<FormErrors> {
    let mut errors = FormErrors::default();
    match error {
        StoreError::DuplicateSlug(slug) => errors.insert(
            "slug",
            format!("Another page already uses the slug `{slug}`."),
        ),
        StoreError::UnknownMonitors(keys) => errors.insert(
            "layout",
            format!(
                "No monitors with the keys {}.",
                keys.iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        StoreError::PageNotFound(_) => return Err(not_found().into()),
        other => return Err(other.into()),
    }
    Ok(errors)
}

async fn editor_page(
    cx: &Cx,
    admin: &AdminUser,
    title: &str,
    input: &PageForm,
    errors: &FormErrors,
    action: &str,
) -> Result<Response> {
    let keys = monitor_keys(cx).await?;
    let html = view! { cx =>
        admin_shell(title: title, admin: admin, section: "pages",
            <div class="page-head"><h1>(title)</h1></div>
            <div class="card">page_editor(form: input, errors: errors, action: action, monitor_keys: &keys)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}

/// Every monitor and page as TOML: the format `uptimestatus seed` reads.
#[route(GET "/admin/export.toml")]
pub(crate) async fn export_toml(cx: &Cx) -> Result<Response> {
    require_admin(cx).await?;
    let text = app(cx)
        .store
        .export_config()
        .await?
        .to_toml()
        .map_err(|e| topcoat::Error::from(std::io::Error::other(e)))?;
    (
        [
            (
                topcoat::router::header::CONTENT_TYPE,
                "application/toml; charset=utf-8",
            ),
            (
                topcoat::router::header::CONTENT_DISPOSITION,
                "attachment; filename=\"uptimestatus.toml\"",
            ),
        ],
        text,
    )
        .into_response(cx)
}
