#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Group monitors: membership, cycle checks and their scheduling helpers.

mod common;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{CheckSpec, GroupCheck, MonitorSpec, MonitorState};
use uptime_store::{Store, StoreError};
use uptime_testkit::TestDb;

fn in_group(key: &str, group: &str) -> MonitorSpec {
    MonitorSpec {
        group: Some(group.into()),
        ..tcp_spec(key)
    }
}

fn group(key: &str, folder: Option<&str>, path: &str) -> MonitorSpec {
    MonitorSpec {
        check: CheckSpec::Group(GroupCheck::path(path)),
        group: folder.map(Into::into),
        ..tcp_spec(key)
    }
}

async fn keys(store: &Store, group: uptime_domain::MonitorId) -> Vec<String> {
    store
        .group_members(group)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.monitor.spec.key.to_string())
        .collect()
}

#[tokio::test]
async fn group_members_are_the_monitors_in_and_below_the_path() {
    let db = TestDb::new().await;
    let store = db.store();
    for spec in [
        in_group("api", "prod"),
        in_group("db", "prod/eu"),
        in_group("web", "dev"),
        in_group("prodx", "production"),
        tcp_spec("loose"),
    ] {
        store.create_monitor(&spec, t(0)).await.unwrap();
    }
    let prod = store
        .create_monitor(&group("prod-all", Some("ops"), "prod"), t(0))
        .await
        .unwrap();
    let eu = store
        .create_monitor(&group("eu-all", Some("prod"), "prod/eu"), t(0))
        .await
        .unwrap();

    assert_eq!(keys(store, prod.id).await, ["api", "db", "eu-all"]);
    assert_eq!(keys(store, eu.id).await, ["db"]);
    let api = store.list_monitors().await.unwrap().remove(0);
    assert!(store.group_members(api.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn group_monitors_cannot_include_each_other() {
    let db = TestDb::new().await;
    let store = db.store();
    store
        .create_monitor(&group("a", Some("x"), "y"), t(0))
        .await
        .unwrap();
    let cycle = store
        .create_monitor(&group("b", Some("y"), "x"), t(0))
        .await
        .unwrap_err();
    assert!(
        matches!(&cycle, StoreError::GroupCycle(keys) if keys == &["a", "b"]),
        "{cycle:?}"
    );
    assert_eq!(
        cycle.to_string(),
        "group monitors would include each other: a, b"
    );

    // Editing into a cycle is refused too; a harmless edit is fine.
    let b = store
        .create_monitor(&group("b", Some("z"), "x"), t(0))
        .await
        .unwrap();
    assert!(matches!(
        store
            .update_monitor(b.id, &group("b", Some("y"), "x"), t(1))
            .await,
        Err(StoreError::GroupCycle(_))
    ));
    store
        .update_monitor(b.id, &group("b", Some("w"), "x"), t(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_group_without_members_is_unknown_and_records_no_check() {
    let db = TestDb::new().await;
    let store = db.store();
    let empty = store
        .create_monitor(&group("empty", None, "nothing"), t(0))
        .await
        .unwrap();
    let claim = store.claim_due(t(1), 10, "test").await.unwrap().remove(0);

    store
        .record_unknown(claim.monitor.id, t(2), t(62), "the group has no members")
        .await
        .unwrap();

    let runtime = store.runtime(empty.id).await.unwrap().unwrap();
    assert_eq!(runtime.runtime.state, MonitorState::Unknown);
    assert_eq!(runtime.next_run_at, t(62));
    assert_eq!(runtime.claimed_by, None);
    assert_eq!(runtime.last_checked_at, Some(t(2)));
    assert_eq!(
        runtime.last_error.as_deref(),
        Some("the group has no members")
    );
    assert!(store.recent_checks(empty.id, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn member_changes_make_covering_groups_due() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store
        .create_monitor(&in_group("api", "prod/eu"), t(0))
        .await
        .unwrap();
    let covering = store
        .create_monitor(&group("prod-all", None, "prod"), t(0))
        .await
        .unwrap();
    let busy = store
        .create_monitor(&group("eu-all", None, "prod/eu"), t(0))
        .await
        .unwrap();
    let elsewhere = store
        .create_monitor(&group("dev-all", None, "dev"), t(0))
        .await
        .unwrap();
    // Everything ran at t(0) and is next due at t(60); `eu-all` is mid-check.
    for claim in store.claim_due(t(0), 10, "test").await.unwrap() {
        if claim.monitor.id != busy.id {
            store
                .record_unknown(claim.monitor.id, t(0), t(60), "idle")
                .await
                .unwrap();
        }
    }

    let expedited = store.expedite_groups(api.id, t(10)).await.unwrap();

    assert_eq!(expedited, 1);
    let due = |id| async move { store.runtime(id).await.unwrap().unwrap().next_run_at };
    assert_eq!(due(covering.id).await, t(10));
    assert_eq!(due(elsewhere.id).await, t(60));
    assert_ne!(due(busy.id).await, t(10), "claimed groups are left alone");
}
