//! Component statuses on a status page: set a manual component's status, or
//! override (and release) a monitor component's, with an optional note.

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;
use topcoat::{
    Result,
    context::Cx,
    router::{
        content::Form,
        error::{SeeOther, not_found, see_other},
        page, path_param, route,
    },
    view::{View, component, view},
};
use uptime_domain::{ComponentSpec, ComponentStatus};
use uptime_store::{ComponentStatusEvent, Page, StoreError};

use super::Id;
use crate::{
    audit,
    auth::{app, require_admin},
    status::component_pill,
    views::{admin_shell, ago},
};

path_param!(key);

/// One component as the console shows it.
pub(crate) struct ComponentRow {
    pub(crate) key: String,
    pub(crate) name: String,
    manual: bool,
    /// The status in force: manual, or an active override.
    status: Option<ComponentStatus>,
    /// The latest change: when, who, and the note.
    latest: Option<(Timestamp, String, Option<String>)>,
}

/// The page's components with their statuses, in layout order.
pub(crate) async fn rows(cx: &Cx, page: &Page) -> Result<Vec<ComponentRow>> {
    let store = &app(cx).store;
    let events = store.component_events(page.id, usize::MAX).await?;
    let names = store.list_monitors().await?;
    Ok(page
        .spec
        .components()
        .map(|component| {
            let key = component.key().as_str();
            let latest = events.iter().find(|event| event.key == key);
            let name = component.label().map_or_else(
                || match component {
                    ComponentSpec::Monitor { monitor, .. } => names
                        .iter()
                        .find(|m| &m.spec.key == monitor)
                        .map_or_else(|| key.to_owned(), |m| m.spec.name.clone()),
                    ComponentSpec::Manual { .. } => key.to_owned(),
                },
                str::to_owned,
            );
            ComponentRow {
                key: key.to_owned(),
                name,
                manual: component.is_manual(),
                status: latest.and_then(|event| event.status),
                latest: latest.map(|e| (e.at, e.actor.clone(), e.note.clone())),
            }
        })
        .collect())
}

/// The status controls: one click sets a status (with the optional note).
#[component]
pub(crate) async fn component_status_list(
    page_id: i64,
    rows: &[ComponentRow],
    /// Where to come back to after a change.
    back: &str,
) -> Result<impl View> {
    let now = Timestamp::now();
    Ok(view! {
        if rows.is_empty() {
            <p class="muted">"No components yet. Add monitors or `manual:key` lines to the layout."</p>
        } else {
            <ul class="component-statuses">
                for row in rows {
                    <li>
                        <form method="post" action=(format!("/admin/pages/{page_id}/components/{}/status", row.key))>
                            <input type="hidden" name="back" value=(back)>
                            <div class="row">
                                <strong>(row.name.as_str())</strong>
                                <code class="small">(row.key.as_str())</code>
                                <span class="small muted">(if row.manual { "manual" } else { "monitor" })</span>
                                <span class="spacer"></span>
                                if let Some(status) = row.status {
                                    <span class=(component_pill(status).0)>
                                        (if row.manual { "" } else { "Override: " }) (component_pill(status).1)
                                    </span>
                                } else {
                                    <span class="small muted">"Follows its checks"</span>
                                }
                            </div>
                            if let Some((at, actor, note)) = &row.latest {
                                <p class="small muted">
                                    "Set " ago(at: *at, now: now) " by " (actor.as_str())
                                    if let Some(note) = note { " · " (note.as_str()) }
                                </p>
                            }
                            <div class="row status-buttons">
                                <input type="text" name="note" placeholder="Note (optional)" aria-label=(format!("Note for {}", row.name))>
                                for status in ComponentStatus::ALL {
                                    <button class="btn btn-small" type="submit" name="status" value=(status.as_str())
                                        aria-pressed=((row.status == Some(status)).then_some("true"))>(status.label())</button>
                                }
                                if !row.manual && row.status.is_some() {
                                    <button class="btn btn-small btn-link" type="submit" name="status" value="clear">"Clear override"</button>
                                }
                            </div>
                        </form>
                    </li>
                }
            </ul>
        }
    })
}

#[page("/admin/pages/{id}/components")]
pub(crate) async fn page_components(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let id = *path_param::<Id>(cx)?;
    let store = &app(cx).store;
    let page = store.page(id).await?.ok_or_else(not_found)?;
    let rows = rows(cx, &page).await?;
    let history: Vec<ComponentStatusEvent> = store.component_events(id, 50).await?;
    let back = format!("/admin/pages/{id}/components");
    let now = Timestamp::now();
    let title = format!("{} components", page.spec.title);
    Ok(view! {
        admin_shell(title: &title, admin: &admin, section: "pages",
            <div class="page-head">
                <h1>"Components"</h1>
                <span class="small muted">(page.spec.title.as_str())</span>
                <span class="spacer"></span>
                <a class="btn" href=(format!("/admin/pages/{id}"))>"Edit page"</a>
                <a class="btn" href=(format!("/s/{}", page.spec.slug))>"View page"</a>
            </div>
            <div class="card">
                <p class="small muted">
                    "Manual components show the status you set. An override replaces a monitor's status on this page until you clear it. "
                    "Both count toward uptime: degraded as up, a partial outage as half down, a major outage as down; maintenance is excluded."
                </p>
                component_status_list(page_id: id, rows: &rows, back: &back)
            </div>
            <div class="card table-wrap">
                <h2>"History"</h2>
                if history.is_empty() {
                    <p class="muted">"No changes yet."</p>
                } else {
                    <table class="list">
                        <thead><tr><th>"When"</th><th>"Component"</th><th>"Status"</th><th class="hide-sm">"By"</th><th>"Note"</th></tr></thead>
                        <tbody>
                            for event in &history {
                                <tr>
                                    <td class="small when">ago(at: event.at, now: now)</td>
                                    <td><code>(event.key.as_str())</code></td>
                                    <td class="small">
                                        (event.previous.map_or("—", ComponentStatus::label))
                                        " → "
                                        (event.status.map_or("follows checks", ComponentStatus::label))
                                    </td>
                                    <td class="small hide-sm">(event.actor.as_str())</td>
                                    <td class="small">(event.note.clone().unwrap_or_default())</td>
                                </tr>
                            }
                        </tbody>
                    </table>
                }
            </div>
        )
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct StatusForm {
    status: String,
    note: String,
    back: String,
}

#[route(POST "/admin/pages/{id}/components/{key}/status")]
pub(crate) async fn set_component_status(
    cx: &Cx,
    Form(input): Form<StatusForm>,
) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = *path_param::<Id>(cx)?;
    let key = path_param::<Key>(cx).to_owned();
    let status = match input.status.as_str() {
        "clear" => None,
        text => Some(text.parse::<ComponentStatus>().map_err(|_| not_found())?),
    };
    let state = app(cx);
    let page = state.store.page(id).await?.ok_or_else(not_found)?;
    let note = Some(input.note.trim()).filter(|n| !n.is_empty());
    let event = match state
        .store
        .set_component_status(
            id,
            &key,
            status,
            note,
            &audit::admin(&admin),
            Timestamp::now(),
        )
        .await
    {
        Ok(event) => event,
        Err(
            StoreError::PageNotFound(_)
            | StoreError::ComponentNotFound(_)
            | StoreError::StatusRequired(_),
        ) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    };
    state.page_changed(page.spec.slug.as_str());
    record(state, audit::admin(&admin), &page, &event).await?;
    // Only back to the console's own page screens.
    let back = if input.back.starts_with(&format!("/admin/pages/{id}")) {
        input.back
    } else {
        format!("/admin/pages/{id}/components")
    };
    Ok(see_other(back))
}

/// Audits a status change (console or API).
pub(crate) async fn record(
    state: &crate::AppState,
    actor: String,
    page: &Page,
    event: &ComponentStatusEvent,
) -> Result<()> {
    let shown = |status: Option<ComponentStatus>| {
        status.map_or_else(|| "follows checks".to_owned(), |s| s.as_str().to_owned())
    };
    audit::record(
        state,
        actor,
        "component.status",
        audit::target("page", page.id),
        format!(
            "Set {} on {} to {}",
            event.key,
            page.spec.title,
            event
                .status
                .map_or("follow its checks", ComponentStatus::label)
        ),
        json!({
            "component": event.key,
            "changed": { "status": { "from": shown(event.previous), "to": shown(event.status) } },
            "note": event.note,
        }),
    )
    .await
}
