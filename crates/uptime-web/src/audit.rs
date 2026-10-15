//! Recording the audit trail (`uptime_store::audit`).
//!
//! Every console mutation and write API call records one event after it
//! succeeds: `audit::record(state, actor, "monitor.update", target, summary,
//! details)`. Details never hold secrets: describe specs with their
//! `redacted()` form, and changes with [`changes`] (which compares the real
//! values but shows the redacted ones, so a changed secret is listed as
//! changed without its value).

use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};
use topcoat::Result;
use uptime_store::{AdminUser, ApiToken, AuditTarget, NewAuditEvent};

use crate::AppState;

/// The actor for a signed-in admin.
pub(crate) fn admin(admin: &AdminUser) -> String {
    admin.login.clone()
}

/// The actor for an API token.
pub(crate) fn api(token: &ApiToken) -> String {
    format!("api:{}", token.name)
}

/// Records an event now. `details` must already be free of secrets.
pub(crate) async fn record(
    state: &AppState,
    actor: impl Into<String>,
    action: &str,
    target: Option<AuditTarget>,
    summary: impl Into<String>,
    details: Value,
) -> Result<()> {
    state
        .store
        .record_audit(
            &NewAuditEvent {
                actor: actor.into(),
                action: action.to_owned(),
                target,
                summary: summary.into(),
                details,
            },
            Timestamp::now(),
        )
        .await?;
    Ok(())
}

pub(crate) fn target(kind: &str, id: impl ToString) -> Option<AuditTarget> {
    Some(AuditTarget::new(kind, id))
}

/// A value as JSON for details (`null` if it cannot be serialized).
pub(crate) fn value(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The fields that differ between `before` and `after`, as
/// `{"changed": {"dotted.path": {"from": …, "to": …}}}`. Paths are found on
/// the real values; what is shown comes from the `shown` (redacted) values.
pub(crate) fn changes<T: Serialize>(
    before: &T,
    after: &T,
    shown_before: &T,
    shown_after: &T,
) -> Value {
    let (before, after) = (flatten(&value(before)), flatten(&value(after)));
    let (shown_before, shown_after) = (flatten(&value(shown_before)), flatten(&value(shown_after)));
    let mut paths: Vec<&String> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut changed = Map::new();
    for path in paths {
        if before.get(path) != after.get(path) {
            changed.insert(
                path.clone(),
                json!({
                    "from": shown_before.get(path).cloned().unwrap_or(Value::Null),
                    "to": shown_after.get(path).cloned().unwrap_or(Value::Null),
                }),
            );
        }
    }
    json!({ "changed": changed })
}

/// [`changes`] for values without secrets.
pub(crate) fn plain_changes<T: Serialize>(before: &T, after: &T) -> Value {
    changes(before, after, before, after)
}

/// A page for details: as data, with custom CSS shortened to its length.
pub(crate) fn page(spec: &uptime_domain::PageSpec) -> Value {
    let mut page = value(spec);
    if let Some(css) = &spec.custom_css {
        page["custom_css"] = json!({ "length": css.len() });
    }
    page
}

/// [`plain_changes`] between two pages, with custom CSS changes listed by
/// length rather than in full.
pub(crate) fn page_changes(
    before: &uptime_domain::PageSpec,
    after: &uptime_domain::PageSpec,
) -> Value {
    let mut details = plain_changes(before, after);
    if before.custom_css != after.custom_css {
        let length = |css: &Option<String>| css.as_ref().map_or(0, String::len);
        details["changed"]["custom_css"] = json!({
            "from": { "length": length(&before.custom_css) },
            "to": { "length": length(&after.custom_css) },
        });
    }
    details
}

/// Whether [`changes`] found anything.
pub(crate) fn changed_fields(details: &Value) -> Vec<String> {
    details["changed"]
        .as_object()
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default()
}

/// Objects flattened to dotted paths; arrays and scalars are leaves.
fn flatten(value: &Value) -> Map<String, Value> {
    fn walk(prefix: &str, value: &Value, out: &mut Map<String, Value>) {
        match value {
            Value::Object(object) if !object.is_empty() => {
                for (key, child) in object {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, child, out);
                }
            }
            leaf => {
                out.insert(prefix.to_owned(), leaf.clone());
            }
        }
    }
    let mut out = Map::new();
    walk("", value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pretty_assertions::assert_eq;
    use uptime_domain::{CheckPolicy, CheckSpec, HttpAuth, HttpCheck, MonitorSpec};

    use super::*;

    fn spec(password: &str, name: &str) -> MonitorSpec {
        let mut http = HttpCheck::get("https://api.example.com/health".parse().unwrap());
        http.auth = Some(HttpAuth::Basic {
            username: "ops".into(),
            password: password.into(),
        });
        MonitorSpec {
            key: "api".parse().unwrap(),
            name: name.into(),
            check: CheckSpec::Http(http),
            policy: CheckPolicy::default(),
            active: true,
            tags: vec![],
            group: None,
        }
    }

    #[test]
    fn changes_name_secret_fields_without_their_values() {
        let (before, after) = (spec("old-secret", "API"), spec("new-secret", "Public API"));
        let details = changes(&before, &after, &before.redacted(), &after.redacted());
        assert_eq!(
            details,
            json!({ "changed": {
                "check.auth.password": { "from": "***", "to": "***" },
                "name": { "from": "API", "to": "Public API" },
            }})
        );
        let text = details.to_string();
        assert!(!text.contains("old-secret") && !text.contains("new-secret"));
        assert_eq!(changed_fields(&details), ["check.auth.password", "name"]);
        assert_eq!(plain_changes(&before, &before), json!({ "changed": {} }));
    }
}
