//! Bulk actions on the dashboard: pause, resume, delete, tag and untag the
//! selected monitors. The page keeps the selection in a Datastar signal and
//! posts it here.

use jiff::Timestamp;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    datastar::{ExecuteScript, PatchElements, Signals},
    router::{
        response::{IntoResponse as _, Response},
        route,
    },
    view::{ViewExt as _, view},
};
use uptime_domain::{MonitorId, TagError, normalize_tags};
use uptime_store::StoreError;

use crate::auth::{app, require_admin};

/// What the dashboard sends: the selected ids and what to do with them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct BulkSignals {
    sel: Vec<String>,
    bulk_action: String,
    bulk_tag: String,
}

/// A bulk action the dashboard offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Pause,
    Resume,
    Delete,
    Tag,
    Untag,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "delete" => Self::Delete,
            "tag" => Self::Tag,
            "untag" => Self::Untag,
            _ => return None,
        })
    }
}

/// `existing` with `change` added or removed, sorted and de-duplicated.
fn retag(existing: &[String], change: &[String], add: bool) -> Result<Vec<String>, TagError> {
    let mut tags: Vec<String> = existing
        .iter()
        .filter(|tag| add || !change.contains(tag))
        .cloned()
        .collect();
    if add {
        tags.extend(change.iter().cloned());
    }
    tags.sort();
    tags.dedup();
    if tags.len() > uptime_domain::MAX_TAGS {
        return Err(TagError::TooMany);
    }
    Ok(tags)
}

async fn notice(cx: &Cx, message: &str) -> Result<Response> {
    let html = view! { cx =>
        <p id="bulk-result" class="small error-text" role="alert">(message)</p>
    }
    .single()
    .await?
    .render(cx);
    PatchElements::new(html).into_response(cx)
}

/// Applies one action to every selected monitor, then reloads the dashboard.
#[route(POST "/admin/monitors/bulk")]
pub(crate) async fn bulk_action(cx: &Cx, Signals(input): Signals<BulkSignals>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let Some(action) = Action::parse(&input.bulk_action) else {
        return notice(cx, "Choose an action.").await;
    };
    let ids: Vec<MonitorId> = input
        .sel
        .iter()
        .filter_map(|id| id.trim().parse().ok().map(MonitorId))
        .collect();
    if ids.is_empty() {
        return notice(cx, "Select at least one monitor.").await;
    }
    let change = if matches!(action, Action::Tag | Action::Untag) {
        match normalize_tags(&input.bulk_tag) {
            Ok(tags) if !tags.is_empty() => tags,
            Ok(_) => return notice(cx, "Enter a tag.").await,
            Err(error) => return notice(cx, &error.to_string()).await,
        }
    } else {
        Vec::new()
    };

    let now = Timestamp::now();
    let mut done = 0;
    for id in &ids {
        let result = match action {
            Action::Pause => state.store.set_monitor_active(*id, false, now).await,
            Action::Resume => state.store.set_monitor_active(*id, true, now).await,
            Action::Delete => state.store.delete_monitor(*id).await.map(drop),
            Action::Tag | Action::Untag => match state.store.monitor(*id).await {
                Ok(Some(monitor)) => {
                    match retag(&monitor.spec.tags, &change, action == Action::Tag) {
                        Ok(tags) => state.store.set_monitor_tags(*id, &tags).await,
                        Err(error) => {
                            return notice(cx, &format!("{}: {error}", monitor.spec.name)).await;
                        }
                    }
                }
                Ok(None) => Err(StoreError::MonitorNotFound(*id)),
                Err(error) => Err(error),
            },
        };
        match result {
            Ok(()) => done += 1,
            Err(StoreError::MonitorNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if matches!(action, Action::Resume) {
        state.wake_scheduler();
    }
    if matches!(action, Action::Delete | Action::Pause | Action::Resume) {
        state.pages.clear();
    }
    tracing::info!(count = done, action = ?action, by = %admin.login, "bulk monitor action");
    ExecuteScript::new("window.location.reload()").into_response(cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn adding_merges_and_removing_subtracts() {
        assert_eq!(
            retag(&tags(&["api", "prod"]), &tags(&["db", "api"]), true),
            Ok(tags(&["api", "db", "prod"]))
        );
        assert_eq!(
            retag(&tags(&["api", "prod"]), &tags(&["prod", "nope"]), false),
            Ok(tags(&["api"]))
        );
    }

    #[test]
    fn too_many_tags_are_refused() {
        let existing: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        assert_eq!(
            retag(&existing, &tags(&["extra"]), true),
            Err(TagError::TooMany)
        );
    }

    #[test]
    fn actions_parse() {
        assert_eq!(Action::parse("untag"), Some(Action::Untag));
        assert_eq!(Action::parse("explode"), None);
    }
}
