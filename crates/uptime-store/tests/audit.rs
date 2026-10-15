#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The audit trail: recorded, filtered and paged newest first.

mod common;

use common::t;
use pretty_assertions::assert_eq;
use serde_json::json;
use uptime_store::{AuditQuery, AuditTarget, NewAuditEvent, Store};
use uptime_testkit::TestDb;

async fn record(store: &Store, actor: &str, action: &str, target: Option<(&str, i64)>, at: i64) {
    store
        .record_audit(
            &NewAuditEvent {
                actor: actor.into(),
                action: action.into(),
                target: target.map(|(kind, id)| AuditTarget::new(kind, id)),
                summary: format!("{actor} did {action}"),
                details: json!({ "at": at }),
            },
            t(at),
        )
        .await
        .unwrap();
}

fn actions(events: &[uptime_store::AuditEvent]) -> Vec<&str> {
    events.iter().map(|e| e.action.as_str()).collect()
}

#[tokio::test]
async fn events_are_listed_newest_first_with_filters_and_pages() {
    let db = TestDb::new().await;
    let store = db.store();
    record(store, "alice", "monitor.create", Some(("monitor", 1)), 10).await;
    record(store, "alice", "monitor.update", Some(("monitor", 1)), 20).await;
    record(store, "api:deploy", "page.update", Some(("page", 7)), 30).await;
    record(store, "bob", "monitor_group.rename", None, 40).await;
    record(store, "bob", "auth.login", None, 50).await;

    let all = store
        .audit_events(&AuditQuery {
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        actions(&all),
        [
            "auth.login",
            "monitor_group.rename",
            "page.update",
            "monitor.update",
            "monitor.create"
        ]
    );
    assert_eq!(all[2].actor, "api:deploy");
    assert_eq!(all[2].target, Some(AuditTarget::new("page", 7)));
    assert_eq!(all[2].details, json!({ "at": 30 }));
    assert_eq!(all[2].at, t(30));
    assert_eq!(all[0].target, None);

    // A prefix matches whole dotted segments only.
    let monitors = store
        .audit_events(&AuditQuery {
            action: Some("monitor".into()),
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(actions(&monitors), ["monitor.update", "monitor.create"]);
    let exact = store
        .audit_events(&AuditQuery {
            action: Some("monitor.update".into()),
            actor: Some("alice".into()),
            target_kind: Some("monitor".into()),
            target_id: Some("1".into()),
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(actions(&exact), ["monitor.update"]);

    // Pages continue below the last id seen.
    let first = store
        .audit_events(&AuditQuery {
            limit: 2,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    let next = store
        .audit_events(&AuditQuery {
            limit: 2,
            before: Some(first[1].id),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(actions(&next), ["page.update", "monitor.update"]);

    assert_eq!(
        store.audit_actors().await.unwrap(),
        ["alice", "api:deploy", "bob"]
    );

    // Retention never touches the trail.
    store.prune_checks_before(t(1_000_000), 100).await.unwrap();
    store.prune_deliveries_before(t(1_000_000)).await.unwrap();
    store.prune_sessions(t(1_000_000)).await.unwrap();
    assert_eq!(
        store
            .audit_events(&AuditQuery {
                limit: 10,
                ..AuditQuery::default()
            })
            .await
            .unwrap()
            .len(),
        5
    );
}
