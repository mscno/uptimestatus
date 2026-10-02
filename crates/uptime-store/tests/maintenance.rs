#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{MaintenanceSpec, MonitorId};
use uptime_store::StoreError;
use uptime_testkit::TestDb;

fn window(title: &str, starts: i64, ends: i64, monitors: &[&str]) -> MaintenanceSpec {
    MaintenanceSpec {
        title: title.into(),
        description: Some("Planned upgrade".into()),
        starts_at: t(starts),
        ends_at: t(ends),
        repeat: None,
        repeat_until: None,
        monitors: monitors.iter().map(|k| k.parse().unwrap()).collect(),
    }
}

#[tokio::test]
async fn windows_are_created_listed_updated_and_deleted() {
    let db = TestDb::new().await;
    let store = db.store();
    let a = store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    let b = store.create_monitor(&tcp_spec("b"), t(0)).await.unwrap();

    let created = store
        .create_maintenance(&window("Upgrade", 100, 200, &["b", "a"]), t(0))
        .await
        .unwrap();
    assert_eq!(created.spec, window("Upgrade", 100, 200, &["a", "b"]));
    assert_eq!(created.monitor_ids, [a.id, b.id]);

    let updated = store
        .update_maintenance(created.id, &window("Bigger upgrade", 100, 300, &["b"]))
        .await
        .unwrap();
    assert_eq!(updated.spec.title, "Bigger upgrade");
    assert_eq!(updated.monitor_ids, [b.id]);
    assert_eq!(store.list_maintenances().await.unwrap().len(), 1);

    assert!(store.delete_maintenance(created.id).await.unwrap());
    assert!(!store.delete_maintenance(created.id).await.unwrap());
    assert_eq!(store.maintenance(created.id).await.unwrap(), None);
}

#[tokio::test]
async fn unknown_monitors_are_rejected() {
    let db = TestDb::new().await;
    let error = db
        .store()
        .create_maintenance(&window("Upgrade", 0, 10, &["ghost"]), t(0))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::UnknownMonitors(ref keys) if keys[0].as_str() == "ghost"));
}

#[tokio::test]
async fn only_active_windows_put_monitors_in_maintenance() {
    let db = TestDb::new().await;
    let store = db.store();
    let a = store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    let b = store.create_monitor(&tcp_spec("b"), t(0)).await.unwrap();
    store
        .create_maintenance(&window("Now", 100, 200, &["a"]), t(0))
        .await
        .unwrap();
    store
        .create_maintenance(&window("Later", 500, 600, &["b"]), t(0))
        .await
        .unwrap();

    let at = |s| async move { store.monitors_in_maintenance(t(s)).await.unwrap() };
    assert!(at(99).await.is_empty());
    assert_eq!(at(100).await, [a.id].into_iter().collect());
    assert!(at(200).await.is_empty(), "the end is exclusive");
    assert_eq!(at(550).await, [b.id].into_iter().collect());
}

#[tokio::test]
async fn windows_are_found_by_overlap() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    for (title, starts, ends) in [
        ("past", 0, 50),
        ("now", 90, 150),
        ("soon", 300, 400),
        ("far", 9_000, 9_100),
    ] {
        store
            .create_maintenance(&window(title, starts, ends, &["a"]), t(0))
            .await
            .unwrap();
    }

    let titles: Vec<String> = store
        .maintenances_between(t(100), t(1_000))
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.spec.title)
        .collect();

    assert_eq!(titles, ["now", "soon"]);
}

#[tokio::test]
async fn deleting_a_monitor_removes_it_from_windows() {
    let db = TestDb::new().await;
    let store = db.store();
    let a = store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    store.create_monitor(&tcp_spec("b"), t(0)).await.unwrap();
    let window = store
        .create_maintenance(&window("Upgrade", 0, 100, &["a", "b"]), t(0))
        .await
        .unwrap();

    store.delete_monitor(a.id).await.unwrap();

    let window = store.maintenance(window.id).await.unwrap().unwrap();
    assert_eq!(window.spec.monitors.len(), 1);
    assert!(!window.monitor_ids.contains(&MonitorId(a.id.0)));
}

fn weekly(title: &str, starts: i64, ends: i64, monitors: &[&str]) -> MaintenanceSpec {
    MaintenanceSpec {
        repeat: Some(uptime_domain::Repeat::Weekly),
        repeat_until: Some(t(starts + 3 * WEEK)),
        ..window(title, starts, ends, monitors)
    }
}

const WEEK: i64 = 7 * 24 * 3600;

#[tokio::test]
async fn recurring_windows_round_trip_and_recur() {
    let db = TestDb::new().await;
    let store = db.store();
    let a = store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    let spec = weekly("Patching", 100, 200, &["a"]);
    let created = store.create_maintenance(&spec, t(0)).await.unwrap();
    assert_eq!(created.spec, spec);

    let at = |s| async move { store.monitors_in_maintenance(t(s)).await.unwrap() };
    assert_eq!(at(150).await, [a.id].into_iter().collect());
    assert!(at(300).await.is_empty());
    assert_eq!(at(WEEK + 150).await, [a.id].into_iter().collect());
    assert_eq!(at(3 * WEEK + 150).await, [a.id].into_iter().collect());
    assert!(at(4 * WEEK + 150).await.is_empty(), "past repeat_until");
}

#[tokio::test]
async fn between_lists_each_occurrence_with_its_own_times() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("a"), t(0)).await.unwrap();
    store
        .create_maintenance(&weekly("Patching", 100, 200, &["a"]), t(0))
        .await
        .unwrap();
    store
        .create_maintenance(&window("One-off", WEEK + 500, WEEK + 600, &["a"]), t(0))
        .await
        .unwrap();

    let found = store
        .maintenances_between(t(WEEK), t(2 * WEEK + 1000))
        .await
        .unwrap();

    let shown: Vec<_> = found
        .iter()
        .map(|m| (m.spec.title.as_str(), m.spec.starts_at))
        .collect();
    assert_eq!(
        shown,
        [
            ("Patching", t(WEEK + 100)),
            ("One-off", t(WEEK + 500)),
            ("Patching", t(2 * WEEK + 100)),
        ]
    );
}
