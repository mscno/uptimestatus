//! The audit trail page: who changed what, newest first, with filters.

use std::collections::HashMap;

use jiff::Timestamp;
use topcoat::{
    Result,
    context::Cx,
    router::page,
    view::{View, view},
};
use uptime_domain::MonitorId;
use uptime_store::{AuditEvent, AuditQuery, AuditTarget};

use crate::{
    AppState,
    auth::{app, require_admin},
    views::{admin_shell, ago},
};

/// Events per page.
const PAGE: u32 = 50;

/// The action families offered as filters.
const FAMILIES: [&str; 17] = [
    "auth",
    "bulk",
    "channel",
    "component",
    "config",
    "domain",
    "image",
    "incident",
    "maintenance",
    "monitor",
    "page",
    "report",
    "template",
    "token",
    "uptime",
    "secrets",
    "subscriber",
];

/// `?actor=&action=&target=kind:id&before=`; empty values mean "any".
#[topcoat::router::query_params]
pub(crate) struct AuditFilters {
    actor: Option<String>,
    action: Option<String>,
    target: Option<String>,
    before: Option<String>,
}

fn present(value: Option<&String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

impl AuditFilters {
    fn query(&self) -> AuditQuery {
        let target = present(self.target.as_ref());
        let (target_kind, target_id) = match target.as_deref().map(|t| t.split_once(':')) {
            Some(Some((kind, id))) => (Some(kind.to_owned()), Some(id.to_owned())),
            Some(None) => (target, None),
            None => (None, None),
        };
        AuditQuery {
            actor: present(self.actor.as_ref()),
            action: present(self.action.as_ref()),
            target_kind,
            target_id,
            before: present(self.before.as_ref()).and_then(|b| b.parse().ok()),
            limit: PAGE,
        }
    }

    /// This filter's query string for the page after `last`.
    fn older(&self, last: i64) -> String {
        let mut pairs = Vec::new();
        for (name, value) in [
            ("actor", &self.actor),
            ("action", &self.action),
            ("target", &self.target),
        ] {
            if let Some(value) = present(value.as_ref()) {
                pairs.push(format!("{name}={}", encode(&value)));
            }
        }
        pairs.push(format!("before={last}"));
        format!("/admin/audit?{}", pairs.join("&"))
    }
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// The console page of a target that still exists.
async fn target_link(state: &AppState, target: &AuditTarget) -> Result<Option<String>> {
    let Ok(id) = target.id.parse::<i64>() else {
        return Ok(None);
    };
    let store = &state.store;
    Ok(match target.kind.as_str() {
        "monitor" => store
            .monitor(MonitorId(id))
            .await?
            .map(|_| format!("/admin/monitors/{id}")),
        "page" => store.page(id).await?.map(|_| format!("/admin/pages/{id}")),
        "incident" => store
            .incident(id)
            .await?
            .map(|_| format!("/admin/incidents/{id}")),
        "channel" => store
            .channel(id)
            .await?
            .map(|_| format!("/admin/alerts/{id}")),
        "maintenance" => store
            .maintenance(id)
            .await?
            .map(|_| format!("/admin/maintenance/{id}")),
        "template" => store
            .template(id)
            .await?
            .map(|_| format!("/admin/incidents/templates/{id}/edit")),
        "token" => store
            .list_api_tokens()
            .await?
            .iter()
            .any(|t| t.id == id)
            .then(|| "/admin/api".to_owned()),
        _ => None,
    })
}

/// One row: the event, its target's label and link, and its details as JSON.
struct Row {
    event: AuditEvent,
    target: Option<(String, Option<String>)>,
    details: Option<String>,
}

#[page("/admin/audit")]
pub(crate) async fn audit_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let filters = topcoat::router::query_params::<AuditFilters>(cx).ok();
    let query = filters.map(AuditFilters::query).unwrap_or(AuditQuery {
        limit: PAGE,
        ..AuditQuery::default()
    });
    let events = state.store.audit_events(&query).await?;
    let actors = state.store.audit_actors().await?;
    let older = (events.len() == PAGE as usize)
        .then(|| events.last().map(|e| e.id))
        .flatten()
        .map(|last| match filters {
            Some(filters) => filters.older(last),
            None => format!("/admin/audit?before={last}"),
        });
    let mut links: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut rows = Vec::with_capacity(events.len());
    for event in events {
        let target = match &event.target {
            Some(target) => {
                let key = (target.kind.clone(), target.id.clone());
                let link = match links.get(&key) {
                    Some(link) => link.clone(),
                    None => {
                        let link = target_link(state, target).await?;
                        links.insert(key, link.clone());
                        link
                    }
                };
                Some((format!("{} {}", target.kind, target.id), link))
            }
            None => None,
        };
        let details = (!event.details.is_null())
            .then(|| serde_json::to_string_pretty(&event.details).unwrap_or_default());
        rows.push(Row {
            event,
            target,
            details,
        });
    }
    let chosen_actor = query.actor.clone().unwrap_or_default();
    let chosen_action = query.action.clone().unwrap_or_default();
    let chosen_target = filters
        .and_then(|f| present(f.target.as_ref()))
        .unwrap_or_default();
    let filtered = query.actor.is_some() || query.action.is_some() || query.target_kind.is_some();
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Audit trail", admin: &admin, section: "audit",
            <div class="page-head"><h1>"Audit trail"</h1></div>
            <p class="muted">"Every change made in the console or through the API, newest first. Secrets are never recorded."</p>
            <form class="card row filters" method="get" action="/admin/audit">
                <div class="field">
                    <label for="filter-actor">"Actor"</label>
                    <select id="filter-actor" name="actor">
                        <option value="">"Anyone"</option>
                        for actor in &actors {
                            <option value=(actor.as_str()) selected=((*actor == chosen_actor).then_some(""))>(actor.as_str())</option>
                        }
                    </select>
                </div>
                <div class="field">
                    <label for="filter-action">"Action"</label>
                    <select id="filter-action" name="action">
                        <option value="">"Any"</option>
                        for family in FAMILIES {
                            <option value=(family) selected=((family == chosen_action).then_some(""))>(family)</option>
                        }
                    </select>
                </div>
                <div class="field">
                    <label for="filter-target">"Target"</label>
                    <input type="text" id="filter-target" name="target" value=(chosen_target.as_str())
                        placeholder="monitor:12">
                </div>
                <div class="actions">
                    <button class="btn btn-primary" type="submit">"Filter"</button>
                    if filtered { <a class="btn" href="/admin/audit">"Clear"</a> }
                </div>
            </form>
            if rows.is_empty() {
                <div class="card empty"><p><strong>"No events."</strong></p></div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"When"</th><th>"Actor"</th><th>"Action"</th><th>"What"</th></tr></thead>
                        <tbody>
                            for row in &rows {
                                <tr>
                                    <td class="small when">ago(at: row.event.at, now: now)</td>
                                    <td class="small">(row.event.actor.as_str())</td>
                                    <td><code>(row.event.action.as_str())</code></td>
                                    <td>
                                        (row.event.summary.as_str())
                                        if let Some((label, link)) = &row.target {
                                            " "
                                            if let Some(link) = link {
                                                <a class="small" href=(link.as_str())>(label.as_str())</a>
                                            } else {
                                                <span class="small muted">(label.as_str()) " (deleted)"</span>
                                            }
                                        }
                                        if let Some(details) = &row.details {
                                            <details><summary class="small">"Details"</summary><pre class="response">(details.as_str())</pre></details>
                                        }
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
                if let Some(older) = &older {
                    <p><a class="btn" href=(older.as_str())>"Older events"</a></p>
                }
            }
        )
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn filters_become_queries_and_keep_themselves_across_pages() {
        let filters = AuditFilters {
            actor: Some("alice".into()),
            action: Some(" ".into()),
            target: Some("monitor:12".into()),
            before: Some("40".into()),
        };
        assert_eq!(
            filters.query(),
            AuditQuery {
                actor: Some("alice".into()),
                action: None,
                target_kind: Some("monitor".into()),
                target_id: Some("12".into()),
                before: Some(40),
                limit: PAGE,
            }
        );
        assert_eq!(
            filters.older(7),
            "/admin/audit?actor=alice&target=monitor%3A12&before=7"
        );
    }
}
