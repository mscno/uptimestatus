#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Manual components and status overrides on status pages.

mod common;

use common::{t, tcp_spec};
use jiff::{ToSpan as _, tz::TimeZone};
use pretty_assertions::assert_eq;
use uptime_domain::{
    ComponentSpec, ComponentStatus, Health, MonitorState, PageSpec, Runtime, SectionSpec, Theme,
    Verdict,
};
use uptime_store::{CheckRecord, Store, StoreError};
use uptime_testkit::TestDb;

fn page(components: Vec<ComponentSpec>) -> PageSpec {
    PageSpec {
        slug: "platform".parse().unwrap(),
        title: "Platform".into(),
        description: None,
        accent: None,
        theme: Theme::Auto,
        look: Default::default(),
        published: true,
        website: None,
        custom_css: None,
        sections: vec![SectionSpec {
            name: "Core".into(),
            components,
        }],
    }
}

fn api() -> ComponentSpec {
    ComponentSpec::of_monitor("api".parse().unwrap(), Some("API".into()))
}

fn cdn() -> ComponentSpec {
    ComponentSpec::manual(
        "cdn".parse().unwrap(),
        Some("CDN".into()),
        Some("Static assets".into()),
    )
}

async fn setup(db: &TestDb) -> (&Store, i64) {
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let page = store.create_page(&page(vec![api(), cdn()])).await.unwrap();
    (store, page.id)
}

#[tokio::test]
async fn manual_components_keep_their_place_and_start_operational() {
    let db = TestDb::new().await;
    let (store, id) = setup(&db).await;
    let saved = store.page(id).await.unwrap().unwrap();
    assert_eq!(saved.spec.sections[0].components, [api(), cdn()]);

    let changes = store.component_changes(id).await.unwrap();
    assert_eq!(changes.len(), 1, "only manual components start a history");
    assert_eq!(changes["cdn"][0].status, Some(ComponentStatus::Operational));

    // Reordering and saving keeps the history (it is keyed by component key).
    store
        .update_page(id, &page(vec![cdn(), api()]))
        .await
        .unwrap();
    let saved = store.page(id).await.unwrap().unwrap();
    assert_eq!(saved.spec.sections[0].components, [cdn(), api()]);
    assert_eq!(store.component_changes(id).await.unwrap()["cdn"].len(), 1);
    assert_eq!(store.list_pages().await.unwrap()[0].components, 2);
}

#[tokio::test]
async fn invalid_pages_are_refused() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let clash = ComponentSpec::manual("api".parse().unwrap(), None, None);
    assert!(matches!(
        store.create_page(&page(vec![api(), clash])).await,
        Err(StoreError::InvalidPage(_))
    ));
    let mut css = page(vec![api()]);
    css.custom_css = Some("a { background: url(javascript:x) }".into());
    assert!(matches!(
        store.create_page(&css).await,
        Err(StoreError::InvalidPage(_))
    ));
}

#[tokio::test]
async fn statuses_and_overrides_are_recorded_with_who_and_why() {
    let db = TestDb::new().await;
    let (store, id) = setup(&db).await;

    let event = store
        .set_component_status(
            id,
            "cdn",
            Some(ComponentStatus::Degraded),
            Some("Slow in Asia"),
            "alice",
            t(100),
        )
        .await
        .unwrap();
    assert_eq!(
        (event.key.as_str(), event.status, event.previous),
        (
            "cdn",
            Some(ComponentStatus::Degraded),
            Some(ComponentStatus::Operational)
        )
    );
    assert_eq!(event.note.as_deref(), Some("Slow in Asia"));

    // A monitor component can be overridden, and the override cleared.
    store
        .set_component_status(
            id,
            "api",
            Some(ComponentStatus::PartialOutage),
            None,
            "bob",
            t(200),
        )
        .await
        .unwrap();
    let cleared = store
        .set_component_status(id, "api", None, Some("Fixed"), "bob", t(300))
        .await
        .unwrap();
    assert_eq!(cleared.previous, Some(ComponentStatus::PartialOutage));

    let changes = store.component_changes(id).await.unwrap();
    let statuses: Vec<_> = changes["api"].iter().map(|c| c.status).collect();
    assert_eq!(statuses, [Some(ComponentStatus::PartialOutage), None]);

    let events = store.component_events(id, 10).await.unwrap();
    let summary: Vec<(&str, &str)> = events
        .iter()
        .map(|e| (e.key.as_str(), e.actor.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            ("api", "bob"),
            ("api", "bob"),
            ("cdn", "alice"),
            ("cdn", "system")
        ],
        "newest first"
    );

    // Manual components always have a status; unknown keys are refused.
    assert!(matches!(
        store
            .set_component_status(id, "cdn", None, None, "alice", t(400))
            .await,
        Err(StoreError::StatusRequired(_))
    ));
    assert!(matches!(
        store
            .set_component_status(
                id,
                "nope",
                Some(ComponentStatus::Degraded),
                None,
                "alice",
                t(400)
            )
            .await,
        Err(StoreError::ComponentNotFound(_))
    ));
    assert!(matches!(
        store
            .set_component_status(
                999,
                "cdn",
                Some(ComponentStatus::Degraded),
                None,
                "alice",
                t(400)
            )
            .await,
        Err(StoreError::PageNotFound(999))
    ));
}

#[tokio::test]
async fn daily_tallies_come_from_status_history_and_blend_into_checks() {
    let db = TestDb::new().await;
    let (store, id) = setup(&db).await;
    let monitor = store.list_monitors().await.unwrap().remove(0);
    let day = t(0).to_zoned(TimeZone::UTC).date();
    let start = day.to_zoned(TimeZone::UTC).unwrap().timestamp();
    let at = |hours: i64| start + jiff::SignedDuration::from_hours(hours);
    // The API passed every check that day.
    for hour in 0..24 {
        store
            .record_check(&CheckRecord {
                monitor_id: monitor.id,
                scheduled_for: at(hour),
                checked_at: at(hour),
                verdict: Verdict {
                    health: Health::Up,
                    latency: None,
                    status_code: None,
                    reason: None,
                    response_body: None,
                },
                runtime: Runtime {
                    state: MonitorState::Up,
                    consecutive_failures: 0,
                },
                transition: None,
                cert: None,
                next_run_at: at(hour + 1),
                region: "test".into(),
            })
            .await
            .unwrap();
    }
    // Six hours of major outage declared on the API, and on the CDN.
    for key in ["api", "cdn"] {
        store
            .set_component_status(
                id,
                key,
                Some(ComponentStatus::MajorOutage),
                None,
                "alice",
                at(6),
            )
            .await
            .unwrap();
    }
    store
        .set_component_status(id, "api", None, None, "alice", at(12))
        .await
        .unwrap();
    store
        .set_component_status(
            id,
            "cdn",
            Some(ComponentStatus::Operational),
            None,
            "alice",
            at(12),
        )
        .await
        .unwrap();
    let next = day.checked_add(1.day()).unwrap();
    let now = at(48);

    let api = store
        .component_daily_tallies(id, "api", day, next, now)
        .await
        .unwrap();
    assert_eq!(api[&day].uptime(), Some(0.75));
    let cdn = store
        .component_daily_tallies(id, "cdn", day, next, now)
        .await
        .unwrap();
    // The CDN's history starts when the page was created (now-ish in this
    // test's clock), so only the explicitly set statuses count that day:
    // six hours down, then operational.
    assert_eq!(cdn[&day].down, 6 * 3600);
    assert!(cdn.contains_key(&next), "operational since");
    assert!(matches!(
        store
            .component_daily_tallies(id, "nope", day, next, now)
            .await,
        Err(StoreError::ComponentNotFound(_))
    ));
}
