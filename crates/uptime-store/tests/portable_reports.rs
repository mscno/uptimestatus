#![allow(clippy::unwrap_used)]

//! Templates, scheduled updates and report storage on SQLite and Turso, whose
//! raw SQL differs from PostgreSQL's.

mod common;

use common::{http_spec, t};
use uptime_domain::{
    ComponentSpec, Health, Impact, IncidentStatus, IncidentTemplate, MonitorState, PageSpec,
    Runtime, SectionSpec, Theme, Verdict,
};
use uptime_store::{
    Backend, CheckRecord, ConnectOptions, NewIncident, NewScheduledUpdate, ReportSettings,
    ScheduledKind, Store,
};

async fn connect(url: &str, backend: Backend) -> Store {
    Store::connect_backend(
        url,
        backend,
        None,
        &ConnectOptions {
            max_connections: 1,
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
}

fn temporary_path(backend: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "uptimestatus-reports-{backend}-{}-{nonce}.db",
        std::process::id()
    ))
}

#[tokio::test]
async fn templates_schedules_and_reports_work_on_sqlite_and_turso() {
    for (scheme, backend) in [("sqlite", Backend::Sqlite), ("turso", Backend::Turso)] {
        let path = temporary_path(scheme);
        let store = connect(&format!("{scheme}:{}", path.display()), backend).await;
        store.migrate().await.unwrap();
        let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();

        let template = store
            .create_template(&IncidentTemplate {
                name: "Down".into(),
                title: "{monitor} is down".into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                body: "Looking.".into(),
            })
            .await
            .unwrap();
        assert!(
            store.delete_template(template.id).await.unwrap(),
            "{scheme}"
        );

        let incident = store
            .declare_incident(
                &NewIncident {
                    title: "Slow".into(),
                    impact: Impact::Minor,
                    status: IncidentStatus::Investigating,
                    message: "Looking.".into(),
                    monitors: vec!["api".parse().unwrap()],
                },
                t(10),
            )
            .await
            .unwrap();
        let scheduled = store
            .schedule_update(&NewScheduledUpdate {
                publish_at: t(20),
                status: IncidentStatus::Resolved,
                body: "Fixed.".into(),
                kind: ScheduledKind::Update {
                    incident_id: incident.id,
                },
                actor: "alice".into(),
            })
            .await
            .unwrap();
        let due = store.due_scheduled(t(20), 10).await.unwrap();
        assert_eq!(due, std::slice::from_ref(&scheduled), "{scheme}");
        let resolved = store
            .post_scheduled_update(&scheduled, t(21))
            .await
            .unwrap();
        assert_eq!(resolved.resolved_at, Some(t(21)), "{scheme}");
        assert!(
            store
                .post_scheduled_update(&scheduled, t(22))
                .await
                .is_err(),
            "{scheme}: published once"
        );

        let page = store
            .create_page(&PageSpec {
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
                    components: vec![ComponentSpec::of_monitor("api".parse().unwrap(), None)],
                }],
            })
            .await
            .unwrap();
        let settings = ReportSettings {
            private: true,
            sla_target: Some("99.9".parse().unwrap()),
        };
        store.set_report_settings(page.id, &settings).await.unwrap();
        assert_eq!(store.report_settings(page.id).await.unwrap(), settings);
        let link = store
            .create_report_link(page.id, None, None, None, "alice", "hash")
            .await
            .unwrap();
        assert!(store.delete_report_link(page.id, link.id).await.unwrap());

        for (at, state) in [(30, MonitorState::Pending), (40, MonitorState::Down)] {
            store
                .record_check(&CheckRecord {
                    monitor_id: monitor.id,
                    scheduled_for: t(at),
                    checked_at: t(at),
                    verdict: Verdict {
                        health: Health::Down,
                        latency: None,
                        status_code: None,
                        reason: None,
                        response_body: None,
                    },
                    runtime: Runtime {
                        state,
                        consecutive_failures: 1,
                    },
                    transition: None,
                    cert: None,
                    next_run_at: t(at + 60),
                    region: "test".into(),
                })
                .await
                .unwrap();
        }
        store
            .excuse_checks(monitor.id, t(0), t(100), t(200))
            .await
            .unwrap();
        assert_eq!(
            store
                .excused_checks_between(&[monitor.id], t(0), t(35))
                .await
                .unwrap(),
            1,
            "{scheme}"
        );
        assert_eq!(
            store.first_activity(page.id, &[monitor.id]).await.unwrap(),
            Some(t(30).to_zoned(jiff::tz::TimeZone::UTC).date()),
            "{scheme}"
        );
        assert!(store.delete_page(page.id).await.unwrap());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
