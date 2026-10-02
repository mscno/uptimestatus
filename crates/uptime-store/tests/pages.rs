#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{t, tcp_spec};
use jiff::civil::date;
use pretty_assertions::assert_eq;
use uptime_domain::{
    ComponentSpec, Health, MonitorState, PageSpec, Runtime, SectionSpec, Theme, Verdict,
};
use uptime_store::{CheckRecord, StoreError};
use uptime_testkit::TestDb;

fn component(key: &str, label: Option<&str>) -> ComponentSpec {
    ComponentSpec {
        monitor: key.parse().unwrap(),
        label: label.map(str::to_owned),
    }
}

fn page(slug: &str) -> PageSpec {
    PageSpec {
        slug: slug.parse().unwrap(),
        title: "Platform Status".into(),
        description: Some("Core services".into()),
        accent: Some("#4f46e5".parse().unwrap()),
        theme: Theme::Dark,
        look: uptime_domain::Look::Clean,
        published: true,
        website: None,
        sections: vec![
            SectionSpec {
                name: "Core".into(),
                components: vec![component("api", Some("API")), component("web", None)],
            },
            SectionSpec {
                name: "Data".into(),
                components: vec![component("db", Some("Database"))],
            },
        ],
    }
}

async fn with_monitors(db: &TestDb, keys: &[&str]) {
    for key in keys {
        db.store()
            .create_monitor(&tcp_spec(key), t(0))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn pages_round_trip_with_their_layout() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;

    let created = db.store().create_page(&page("platform")).await.unwrap();
    let by_id = db.store().page(created.id).await.unwrap().unwrap();
    let by_slug = db.store().page_by_slug("platform").await.unwrap().unwrap();

    assert_eq!(created.spec, page("platform"));
    assert_eq!(by_id, created);
    assert_eq!(by_slug, created);
    assert_eq!(db.store().page_by_slug("nope").await.unwrap(), None);
}

#[tokio::test]
async fn unknown_monitors_are_listed() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api"]).await;

    let error = db.store().create_page(&page("platform")).await.unwrap_err();

    match error {
        StoreError::UnknownMonitors(keys) => {
            assert_eq!(
                keys.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                ["web", "db"]
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        db.store().list_pages().await.unwrap().is_empty(),
        "nothing written"
    );
}

#[tokio::test]
async fn slugs_are_unique() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    db.store().create_page(&page("platform")).await.unwrap();

    let error = db.store().create_page(&page("platform")).await.unwrap_err();

    assert!(
        matches!(error, StoreError::DuplicateSlug(ref slug) if slug.as_str() == "platform"),
        "{error:?}"
    );
}

#[tokio::test]
async fn updating_replaces_the_layout() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    let created = db.store().create_page(&page("platform")).await.unwrap();
    let mut spec = page("platform-v2");
    spec.title = "Renamed".into();
    spec.sections = vec![SectionSpec {
        name: "Only".into(),
        components: vec![component("db", None)],
    }];

    let updated = db.store().update_page(created.id, &spec).await.unwrap();

    assert_eq!(updated.spec, spec);
    assert_eq!(
        db.store().page(created.id).await.unwrap().unwrap().spec,
        spec
    );
    assert_eq!(db.store().page_by_slug("platform").await.unwrap(), None);
}

#[tokio::test]
async fn pages_are_listed_with_component_counts() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    db.store().create_page(&page("zeta")).await.unwrap();
    db.store()
        .create_page(&PageSpec {
            sections: vec![],
            ..page("alpha")
        })
        .await
        .unwrap();

    let pages = db.store().list_pages().await.unwrap();

    let summary: Vec<_> = pages
        .iter()
        .map(|p| (p.slug.as_str().to_owned(), p.components))
        .collect();
    assert_eq!(summary, [("alpha".to_owned(), 0), ("zeta".to_owned(), 3)]);
}

#[tokio::test]
async fn deleting_a_monitor_removes_it_from_pages() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    let created = db.store().create_page(&page("platform")).await.unwrap();
    let web = db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "web")
        .unwrap();

    db.store().delete_monitor(web.id).await.unwrap();

    let keys: Vec<_> = db
        .store()
        .page(created.id)
        .await
        .unwrap()
        .unwrap()
        .spec
        .monitor_keys()
        .map(|k| k.to_string())
        .collect();
    assert_eq!(keys, ["api", "db"]);
}

#[tokio::test]
async fn deleting_a_page() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    let created = db.store().create_page(&page("platform")).await.unwrap();

    assert!(db.store().delete_page(created.id).await.unwrap());
    assert!(!db.store().delete_page(created.id).await.unwrap());
    assert_eq!(db.store().page(created.id).await.unwrap(), None);
}

#[tokio::test]
async fn daily_tallies_cover_the_requested_range() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&tcp_spec("api"), t(0))
        .await
        .unwrap();
    // t(0) is 2027-01-15; record one check per day for three days.
    for day in 0..3 {
        db.store()
            .record_check(&CheckRecord {
                monitor_id: monitor.id,
                scheduled_for: t(day * 86_400),
                checked_at: t(day * 86_400),
                verdict: Verdict {
                    health: Health::Up,
                    latency: Some(Duration::from_millis(5)),
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
                next_run_at: t(day * 86_400 + 60),
                region: "test".into(),
            })
            .await
            .unwrap();
    }

    let tallies = db
        .store()
        .daily_tallies(&[monitor.id], date(2027, 1, 16), date(2027, 1, 20))
        .await
        .unwrap();

    let days: Vec<_> = tallies[&monitor.id].keys().copied().collect();
    assert_eq!(days, [date(2027, 1, 16), date(2027, 1, 17)]);
    assert_eq!(tallies[&monitor.id][&date(2027, 1, 16)].up, 1);
}

#[tokio::test]
async fn website_links_round_trip() {
    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    let spec = PageSpec {
        website: Some(uptime_domain::Website {
            url: "https://privatenpm.com/".parse().unwrap(),
            label: Some("Return to PrivateNPM".into()),
        }),
        ..page("platform")
    };

    let created = db.store().create_page(&spec).await.unwrap();

    let loaded = db.store().page(created.id).await.unwrap().unwrap();
    assert_eq!(loaded.spec, spec);
    let cleared = PageSpec {
        website: None,
        ..spec
    };
    db.store().update_page(created.id, &cleared).await.unwrap();
    assert_eq!(
        db.store()
            .page(created.id)
            .await
            .unwrap()
            .unwrap()
            .spec
            .website,
        None
    );
}

#[tokio::test]
async fn images_are_set_replaced_and_kept_across_edits() {
    use uptime_store::PageImage;

    let db = TestDb::new().await;
    with_monitors(&db, &["api", "web", "db"]).await;
    let store = db.store();
    let page = store.create_page(&page("platform")).await.unwrap();
    assert_eq!((page.logo.clone(), page.favicon.clone()), (None, None));

    assert_eq!(
        store
            .set_page_image(page.id, PageImage::Logo, Some("aa.png"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .set_page_image(page.id, PageImage::Logo, Some("bb.svg"))
            .await
            .unwrap()
            .as_deref(),
        Some("aa.png"),
        "returns the previous file"
    );
    store
        .set_page_image(page.id, PageImage::Favicon, Some("cc.png"))
        .await
        .unwrap();
    let updated = store.update_page(page.id, &page.spec).await.unwrap();
    assert_eq!(
        updated.logo.as_deref(),
        Some("bb.svg"),
        "editing the page keeps images"
    );
    assert_eq!(updated.favicon.as_deref(), Some("cc.png"));
    assert!(store.image_in_use("bb.svg").await.unwrap());
    assert!(store.image_in_use("cc.png").await.unwrap());
    assert!(!store.image_in_use("aa.png").await.unwrap());

    store
        .set_page_image(page.id, PageImage::Logo, None)
        .await
        .unwrap();
    let loaded = store.page(page.id).await.unwrap().unwrap();
    assert_eq!(
        (loaded.logo, loaded.favicon.as_deref()),
        (None, Some("cc.png"))
    );
    assert!(matches!(
        store.set_page_image(4242, PageImage::Logo, None).await,
        Err(StoreError::PageNotFound(4242))
    ));
}
