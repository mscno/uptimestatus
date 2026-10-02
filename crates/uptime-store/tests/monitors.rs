#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use common::{http_spec, t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{MonitorId, MonitorSpec, MonitorState, Runtime};
use uptime_store::StoreError;
use uptime_testkit::TestDb;

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = TestDb::new().await;
    let report = db.store().migrate().await.unwrap();
    assert_eq!(report.applied(), 0);
    assert_eq!(
        report.skipped(),
        uptime_store::MIGRATIONS.migrations().len()
    );
    db.store().ping().await.unwrap();
}

#[tokio::test]
async fn created_monitor_round_trips_its_spec() {
    let db = TestDb::new().await;
    let spec = http_spec("api");

    let created = db.store().create_monitor(&spec, t(0)).await.unwrap();
    let loaded = db
        .store()
        .monitor(created.id)
        .await
        .unwrap()
        .expect("monitor exists");

    assert_eq!(loaded.spec, spec);
    assert_eq!(loaded, created);
}

#[tokio::test]
async fn new_monitor_starts_unknown_and_due_at_first_run() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&tcp_spec("db"), t(5))
        .await
        .unwrap();

    let runtime = db
        .store()
        .runtime(monitor.id)
        .await
        .unwrap()
        .expect("runtime row exists");

    assert_eq!(
        runtime.runtime,
        Runtime {
            state: MonitorState::Unknown,
            consecutive_failures: 0
        }
    );
    assert_eq!(runtime.next_run_at, t(5));
    assert_eq!(runtime.scheduled_for, None);
    assert_eq!(runtime.last_checked_at, None);
}

#[tokio::test]
async fn inactive_monitor_starts_paused() {
    let db = TestDb::new().await;
    let spec = uptime_domain::MonitorSpec {
        active: false,
        tags: Vec::new(),
        group: None,
        ..tcp_spec("paused")
    };
    let monitor = db.store().create_monitor(&spec, t(0)).await.unwrap();

    let runtime = db.store().runtime(monitor.id).await.unwrap().unwrap();
    assert_eq!(runtime.runtime.state, MonitorState::Paused);
}

#[tokio::test]
async fn duplicate_keys_are_rejected() {
    let db = TestDb::new().await;
    db.store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();

    let err = db
        .store()
        .create_monitor(&tcp_spec("api"), t(0))
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::DuplicateKey(ref key) if key.as_str() == "api"),
        "{err:?}"
    );
    assert_eq!(db.store().list_monitors().await.unwrap().len(), 1);
}

#[tokio::test]
async fn lists_monitors_ordered_by_key() {
    let db = TestDb::new().await;
    for key in ["web", "api", "db"] {
        db.store()
            .create_monitor(&tcp_spec(key), t(0))
            .await
            .unwrap();
    }

    let keys: Vec<_> = db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.spec.key.to_string())
        .collect();

    assert_eq!(keys, ["api", "db", "web"]);
}

#[tokio::test]
async fn missing_monitor_is_none() {
    let db = TestDb::new().await;
    assert_eq!(db.store().monitor(MonitorId(424_242)).await.unwrap(), None);
    assert_eq!(db.store().runtime(MonitorId(424_242)).await.unwrap(), None);
}

#[tokio::test]
async fn deleting_a_monitor_cascades_to_its_runtime() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&tcp_spec("gone"), t(0))
        .await
        .unwrap();

    assert!(db.store().delete_monitor(monitor.id).await.unwrap());
    assert!(!db.store().delete_monitor(monitor.id).await.unwrap());

    assert_eq!(db.store().monitor(monitor.id).await.unwrap(), None);
    assert_eq!(db.store().runtime(monitor.id).await.unwrap(), None);
}

#[tokio::test]
async fn updating_a_monitor_replaces_its_spec_and_makes_it_due_now() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("api"), t(500))
        .await
        .unwrap();
    let mut spec = tcp_spec("api-tcp");
    spec.name = "Renamed".into();

    let updated = db
        .store()
        .update_monitor(monitor.id, &spec, t(100))
        .await
        .unwrap();

    assert_eq!(updated.id, monitor.id);
    assert_eq!(updated.spec, spec);
    assert_eq!(
        db.store().monitor(monitor.id).await.unwrap().unwrap().spec,
        spec
    );
    assert_eq!(
        db.store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .next_run_at,
        t(100)
    );
}

#[tokio::test]
async fn updating_to_a_taken_key_is_rejected() {
    let db = TestDb::new().await;
    db.store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();
    let web = db
        .store()
        .create_monitor(&http_spec("web"), t(0))
        .await
        .unwrap();

    let err = db
        .store()
        .update_monitor(web.id, &http_spec("api"), t(0))
        .await
        .unwrap_err();

    assert!(matches!(err, StoreError::DuplicateKey(_)), "{err:?}");
}

#[tokio::test]
async fn updating_a_missing_monitor_is_not_found() {
    let db = TestDb::new().await;
    let err = db
        .store()
        .update_monitor(MonitorId(9_999), &http_spec("x"), t(0))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::MonitorNotFound(MonitorId(9_999))),
        "{err:?}"
    );
}

#[tokio::test]
async fn pausing_stops_scheduling_and_resuming_restarts_it() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();

    db.store()
        .set_monitor_active(monitor.id, false, t(10))
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .state,
        MonitorState::Paused
    );
    assert!(
        !db.store()
            .monitor(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .spec
            .active
    );
    assert!(
        db.store()
            .claim_due(t(1000), 10, "w")
            .await
            .unwrap()
            .is_empty()
    );

    db.store()
        .set_monitor_active(monitor.id, true, t(20))
        .await
        .unwrap();
    let runtime = db.store().runtime(monitor.id).await.unwrap().unwrap();
    assert_eq!(runtime.runtime.state, MonitorState::Unknown);
    assert_eq!(runtime.next_run_at, t(20));
    assert_eq!(db.store().claim_due(t(20), 10, "w").await.unwrap().len(), 1);
}

#[tokio::test]
async fn saving_an_inactive_spec_pauses_the_monitor() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("api"), t(0))
        .await
        .unwrap();

    let paused = MonitorSpec {
        active: false,
        tags: Vec::new(),
        group: None,
        ..http_spec("api")
    };
    db.store()
        .update_monitor(monitor.id, &paused, t(5))
        .await
        .unwrap();

    assert_eq!(
        db.store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .state,
        MonitorState::Paused
    );
}

#[tokio::test]
async fn overviews_pair_each_monitor_with_its_runtime() {
    let db = TestDb::new().await;
    db.store()
        .create_monitor(&tcp_spec("web"), t(1))
        .await
        .unwrap();
    db.store()
        .create_monitor(&tcp_spec("api"), t(2))
        .await
        .unwrap();

    let overviews = db.store().monitor_overviews().await.unwrap();

    let summary: Vec<_> = overviews
        .iter()
        .map(|o| (o.monitor.spec.key.to_string(), o.runtime.next_run_at))
        .collect();
    assert_eq!(
        summary,
        [("api".to_owned(), t(2)), ("web".to_owned(), t(1))]
    );
}

#[tokio::test]
async fn tags_round_trip_and_can_be_replaced_alone() {
    let db = TestDb::new().await;
    let store = db.store();
    let spec = MonitorSpec {
        tags: vec!["api".into(), "prod".into()],
        ..http_spec("api")
    };
    let monitor = store.create_monitor(&spec, t(0)).await.unwrap();
    assert_eq!(monitor.spec.tags, ["api", "prod"]);
    assert_eq!(store.monitor(monitor.id).await.unwrap().unwrap().spec, spec);

    store
        .set_monitor_tags(monitor.id, &["team:core".into()])
        .await
        .unwrap();
    let loaded = store.monitor(monitor.id).await.unwrap().unwrap();
    assert_eq!(loaded.spec.tags, ["team:core"]);
    assert_eq!(loaded.spec.name, spec.name, "the rest is untouched");

    store.set_monitor_tags(monitor.id, &[]).await.unwrap();
    assert!(
        store
            .monitor(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .spec
            .tags
            .is_empty()
    );
    assert!(matches!(
        store.set_monitor_tags(MonitorId(9999), &[]).await,
        Err(StoreError::MonitorNotFound(_))
    ));
}

#[tokio::test]
async fn groups_round_trip() {
    let db = TestDb::new().await;
    let store = db.store();
    let spec = MonitorSpec {
        group: Some("prod/eu".into()),
        ..http_spec("api")
    };
    let monitor = store.create_monitor(&spec, t(0)).await.unwrap();
    assert_eq!(monitor.spec.group.as_deref(), Some("prod/eu"));
    assert_eq!(store.monitor(monitor.id).await.unwrap().unwrap().spec, spec);

    let moved = MonitorSpec {
        group: None,
        ..spec
    };
    store
        .update_monitor(monitor.id, &moved, t(1))
        .await
        .unwrap();
    assert_eq!(
        store.monitor(monitor.id).await.unwrap().unwrap().spec.group,
        None
    );
}
