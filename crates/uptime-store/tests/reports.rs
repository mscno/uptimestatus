#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! What monthly reports read: settings, links and month-level facts.

mod common;

use std::time::Duration;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{
    ComponentSpec, Health, Impact, IncidentStatus, MonitorId, MonitorState, Month, PageSpec,
    Runtime, SectionSpec, Theme, Verdict,
};
use uptime_store::{CheckRecord, NewIncident, ReportSettings, Store, StoreError};
use uptime_testkit::TestDb;

fn page(slug: &str, monitors: &[&str]) -> PageSpec {
    PageSpec {
        slug: slug.parse().unwrap(),
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
            components: monitors
                .iter()
                .map(|m| ComponentSpec::of_monitor(m.parse().unwrap(), None))
                .collect(),
        }],
    }
}

async fn check(store: &Store, id: MonitorId, at: i64, state: MonitorState) {
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if state == MonitorState::Up {
                    Health::Up
                } else {
                    Health::Down
                },
                latency: Some(Duration::from_millis(5)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state,
                consecutive_failures: u32::from(state != MonitorState::Up),
            },
            transition: None,
            cert: None,
            next_run_at: t(at + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn report_settings_default_to_public_without_a_target() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let page = store
        .create_page(&page("platform", &["api"]))
        .await
        .unwrap();
    assert_eq!(
        store.report_settings(page.id).await.unwrap(),
        ReportSettings::default()
    );
    let settings = ReportSettings {
        private: true,
        sla_target: Some("99.95".parse().unwrap()),
    };
    store.set_report_settings(page.id, &settings).await.unwrap();
    assert_eq!(store.report_settings(page.id).await.unwrap(), settings);
    assert!(matches!(
        store.report_settings(999).await,
        Err(StoreError::PageNotFound(999))
    ));
}

#[tokio::test]
async fn report_links_are_found_by_hash_scoped_and_revoked() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let page = store
        .create_page(&page("platform", &["api"]))
        .await
        .unwrap();
    let september: Month = "2026-09".parse().unwrap();
    let scoped = store
        .create_report_link(
            page.id,
            Some(september),
            Some(" Acme "),
            Some(t(100)),
            "alice",
            "aa",
        )
        .await
        .unwrap();
    assert_eq!(scoped.label.as_deref(), Some("Acme"));
    let every = store
        .create_report_link(page.id, None, Some(""), None, "alice", "bb")
        .await
        .unwrap();
    assert_eq!(every.label, None);
    assert_eq!(
        store.report_link_by_hash("aa").await.unwrap(),
        Some(scoped.clone())
    );
    assert_eq!(store.report_link_by_hash("zz").await.unwrap(), None);
    assert_eq!(store.report_links(page.id).await.unwrap().len(), 2);

    assert!(scoped.opens(september, t(99)));
    assert!(!scoped.opens(september, t(100)), "expired");
    assert!(!scoped.opens(september.next(), t(0)), "another month");
    assert!(every.opens(september.next(), t(1_000_000)));

    assert!(store.delete_report_link(page.id, scoped.id).await.unwrap());
    assert!(!store.delete_report_link(page.id, scoped.id).await.unwrap());
    // Links go with their page.
    assert!(store.delete_page(page.id).await.unwrap());
    assert_eq!(store.report_link_by_hash("bb").await.unwrap(), None);
}

#[tokio::test]
async fn month_facts_come_from_history() {
    let db = TestDb::new().await;
    let store = db.store();
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let web = store.create_monitor(&tcp_spec("web"), t(0)).await.unwrap();
    let page = store
        .create_page(&page("platform", &["api"]))
        .await
        .unwrap();
    assert_eq!(
        store.first_activity(page.id, &[api.id]).await.unwrap(),
        None
    );

    for (at, state) in [
        (0, MonitorState::Up),
        (60, MonitorState::Down),
        (120, MonitorState::Down),
        (180, MonitorState::Up),
    ] {
        check(store, api.id, at, state).await;
    }
    check(store, web.id, 60, MonitorState::Down).await;
    let day = t(0).to_zoned(jiff::tz::TimeZone::UTC).date();
    assert_eq!(
        store.first_activity(page.id, &[api.id]).await.unwrap(),
        Some(day)
    );

    store
        .excuse_checks(api.id, t(60), t(120), t(500))
        .await
        .unwrap();
    store
        .excuse_checks(web.id, t(0), t(100), t(500))
        .await
        .unwrap();
    assert_eq!(
        store
            .excused_checks_between(&[api.id], t(0), t(1000))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .excused_checks_between(&[api.id, web.id], t(0), t(100))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .excused_checks_between(&[], t(0), t(1000))
            .await
            .unwrap(),
        0
    );

    // Incidents overlapping [100, 200): open ones, and ones resolved inside.
    let declare = |title: &str| NewIncident {
        title: title.into(),
        impact: Impact::Minor,
        status: IncidentStatus::Investigating,
        message: "Looking.".into(),
        monitors: vec!["api".parse().unwrap()],
    };
    let before = store
        .declare_incident(&declare("Before"), t(10))
        .await
        .unwrap();
    store
        .post_incident_update(before.id, IncidentStatus::Resolved, "Done.", t(50))
        .await
        .unwrap();
    let spans = store
        .declare_incident(&declare("Spans"), t(90))
        .await
        .unwrap();
    store
        .post_incident_update(spans.id, IncidentStatus::Resolved, "Done.", t(150))
        .await
        .unwrap();
    store
        .declare_incident(&declare("Open"), t(120))
        .await
        .unwrap();
    store
        .declare_incident(&declare("After"), t(200))
        .await
        .unwrap();
    let titles: Vec<String> = store
        .incidents_overlapping(&[api.id], t(100), t(200))
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.title)
        .collect();
    assert_eq!(titles, ["Open", "Spans"]);
}
