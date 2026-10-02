#![allow(clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{http_spec, t};
use uptime_domain::{
    ChannelKind, ChannelSpec, CheckSpec, ComponentSpec, Health, MaintenanceSpec, MonitorId,
    MonitorState, PageSpec, Push, PushCheck, Runtime, SectionSpec, Theme, Transition, Verdict,
};
use uptime_store::{Backend, CheckRecord, ConnectOptions, Store};
use uptime_testkit::TestDb;

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

async fn exercise(store: &Store, prefix: &str) -> (i64, MonitorId, i64) {
    assert_eq!(store.migrate().await.unwrap().applied(), 0);
    store.ping().await.unwrap();
    let monitor_key = format!("{prefix}api");
    let push_key = format!("{prefix}cron");
    let token = format!("{prefix}secret-token");
    let page_slug = format!("{prefix}platform");
    let monitor = store
        .create_monitor(&http_spec(&monitor_key), t(0))
        .await
        .unwrap();
    assert_eq!(
        store.monitor(monitor.id).await.unwrap(),
        Some(monitor.clone())
    );
    let channel = store
        .create_channel(
            &ChannelSpec {
                name: format!("{prefix}Ops"),
                kind: ChannelKind::Slack,
                url: "https://hooks.slack.com/services/T/B/x".parse().unwrap(),
                secret: None,
                default_on: false,
                routing: Default::default(),
            },
            t(0),
        )
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[channel.id])
        .await
        .unwrap();
    let claims = store.claim_due(t(1), 1, "first").await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].monitor, monitor);
    assert!(store.claim_due(t(2), 1, "second").await.unwrap().is_empty());

    let page = store
        .create_page(&PageSpec {
            slug: page_slug.parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: uptime_domain::Look::Clean,
            published: true,
            website: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![ComponentSpec {
                    monitor: monitor_key.parse().unwrap(),
                    label: None,
                }],
            }],
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .list_pages()
            .await
            .unwrap()
            .iter()
            .find(|summary| summary.id == page.id)
            .unwrap()
            .components,
        1
    );

    let maintenance = store
        .create_maintenance(
            &MaintenanceSpec {
                title: "Upgrade".into(),
                description: None,
                starts_at: t(0),
                ends_at: t(10),
                repeat: None,
                repeat_until: None,
                monitors: vec![monitor_key.parse().unwrap()],
            },
            t(0),
        )
        .await
        .unwrap();
    assert!(
        store
            .monitors_in_maintenance(t(5))
            .await
            .unwrap()
            .contains(&monitor.id)
    );
    assert!(store.delete_maintenance(maintenance.id).await.unwrap());

    store
        .record_check(&CheckRecord {
            monitor_id: monitor.id,
            scheduled_for: t(0),
            checked_at: t(2),
            verdict: Verdict {
                health: Health::Down,
                latency: Some(Duration::from_millis(42)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Down,
                consecutive_failures: 1,
            },
            transition: Some(Transition::WentDown),
            cert: None,
            next_run_at: t(62),
            region: "test".into(),
        })
        .await
        .unwrap();
    assert_eq!(store.recent_checks(monitor.id, 10).await.unwrap().len(), 1);
    assert_eq!(
        store
            .daily_tally(monitor.id, t(2).to_zoned(jiff::tz::TimeZone::UTC).date())
            .await
            .unwrap()
            .unwrap()
            .down,
        1
    );
    let day = t(2).to_zoned(jiff::tz::TimeZone::UTC).date();
    let ranged = store
        .daily_tallies(
            &[monitor.id],
            day.yesterday().unwrap(),
            day.tomorrow().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ranged[&monitor.id][&day].down, 1);
    let claimed = store
        .claim_outbox(t(3), 10, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempts, 1);
    assert!(
        store
            .claim_outbox(t(4), 10, Duration::from_secs(30))
            .await
            .unwrap()
            .is_empty()
    );
    store.mark_delivered(claimed[0].id, t(4)).await.unwrap();
    assert_eq!(store.prune_deliveries_before(t(5)).await.unwrap(), 1);
    assert_eq!(store.prune_checks_before(t(5), 10).await.unwrap(), 1);

    let push_spec = uptime_domain::MonitorSpec {
        check: CheckSpec::Push(PushCheck {
            token: token.clone(),
        }),
        ..http_spec(&push_key)
    };
    let push_monitor = store.create_monitor(&push_spec, t(100)).await.unwrap();
    assert_eq!(
        store
            .record_push(
                &token,
                &Push {
                    at: t(6),
                    up: true,
                    message: None,
                    ping: Some(Duration::from_millis(3)),
                }
            )
            .await
            .unwrap(),
        Some(push_monitor.id)
    );

    assert!(store.delete_monitor(monitor.id).await.unwrap());
    assert_eq!(
        store
            .list_pages()
            .await
            .unwrap()
            .iter()
            .find(|summary| summary.id == page.id)
            .unwrap()
            .components,
        0
    );
    assert!(store.page(page.id).await.unwrap().is_some());
    assert!(store.monitor_channels(monitor.id).await.unwrap().is_empty());
    (page.id, push_monitor.id, channel.id)
}

async fn smoke(url: &str, backend: Backend) {
    let store = connect(url, backend).await;
    assert!(store.migrate().await.unwrap().applied() > 0);
    exercise(&store, "").await;
    drop(store);
    let reopened = connect(url, backend).await;
    assert_eq!(reopened.migrate().await.unwrap().applied(), 0);
    assert_eq!(reopened.list_monitors().await.unwrap().len(), 1);
}

#[tokio::test]
async fn sqlite_backend() {
    let path = temporary_path("sqlite");
    smoke(&format!("sqlite:{}", path.display()), Backend::Sqlite).await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn turso_backend() {
    let path = temporary_path("turso");
    smoke(&format!("turso:{}", path.display()), Backend::Turso).await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn postgres_backend() {
    let db = TestDb::new().await;
    exercise(db.store(), "").await;
}

/// Runs the same store scenario against a real Turso Cloud database. This is
/// opt-in because it needs the repository's database-scoped CI credentials.
#[tokio::test]
#[ignore = "requires UPTIMESTATUS_TURSO_TEST_URL and UPTIMESTATUS_TURSO_TEST_AUTH_TOKEN"]
async fn remote_turso_backend() {
    let url = std::env::var("UPTIMESTATUS_TURSO_TEST_URL").unwrap();
    let token = std::env::var("UPTIMESTATUS_TURSO_TEST_AUTH_TOKEN").unwrap();
    let options = ConnectOptions {
        max_connections: 1,
        acquire_timeout: Duration::from_secs(30),
    };
    let store = Store::connect_backend(&url, Backend::Turso, Some(&token), &options)
        .await
        .unwrap();
    store.migrate().await.unwrap();

    // The CI database is dedicated to this test. Clear artifacts left by an
    // interrupted earlier run before creating uniquely named new ones.
    for page in store.list_pages().await.unwrap() {
        if page.slug.as_str().starts_with("rt-") {
            store.delete_page(page.id).await.unwrap();
        }
    }
    for monitor in store.list_monitors().await.unwrap() {
        if monitor.spec.key.as_str().starts_with("rt-") {
            store.delete_monitor(monitor.id).await.unwrap();
        }
    }
    for channel in store.list_channels().await.unwrap() {
        if channel.spec.name.starts_with("rt-") {
            store.delete_channel(channel.id).await.unwrap();
        }
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("rt-{nonce:x}-");
    let (page_id, monitor_id, channel_id) = exercise(&store, &prefix).await;
    assert_eq!(store.migrate().await.unwrap().applied(), 0);
    assert!(store.delete_page(page_id).await.unwrap());
    assert!(store.delete_monitor(monitor_id).await.unwrap());
    assert!(store.delete_channel(channel_id).await.unwrap());
}

#[tokio::test]
async fn backend_selection_rejects_mismatched_urls_and_tokens() {
    let options = ConnectOptions::default();
    assert_eq!(
        Backend::from_url("libsql://db.example.turso.io").unwrap(),
        Backend::Turso
    );
    assert!(
        Store::connect_backend("sqlite::memory:", Backend::Postgres, None, &options)
            .await
            .is_err()
    );
    assert!(
        Store::connect_backend("sqlite::memory:", Backend::Sqlite, Some("secret"), &options)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn postgres_jsonb_upgrade_preserves_existing_monitor_data() {
    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(&http_spec("legacy"), t(0))
        .await
        .unwrap();
    let (client, connection) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "ALTER TABLE monitors ALTER COLUMN \"check\" TYPE JSONB USING \"check\"::jsonb; \
         ALTER TABLE notification_outbox ALTER COLUMN payload TYPE JSONB USING payload::jsonb",
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!("../toasty/migrations/0018_portable_json.sql"))
        .await
        .unwrap();
    assert_eq!(db.store().monitor(monitor.id).await.unwrap(), Some(monitor));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_and_turso_claims_are_atomic() {
    for (scheme, backend) in [("sqlite", Backend::Sqlite), ("turso", Backend::Turso)] {
        let path = temporary_path(scheme);
        let url = format!("{scheme}:{}", path.display());
        let store = connect(&url, backend).await;
        store.migrate().await.unwrap();
        for i in 0..24 {
            store
                .create_monitor(&http_spec(&format!("m{i}")), t(0))
                .await
                .unwrap();
        }
        let mut workers = Vec::new();
        for i in 0..6 {
            let store = store.clone();
            workers.push(tokio::spawn(async move {
                let mut ids = Vec::new();
                loop {
                    let claims = store
                        .claim_due(t(1), 3, &format!("worker-{i}"))
                        .await
                        .unwrap();
                    if claims.is_empty() {
                        return ids;
                    }
                    ids.extend(claims.into_iter().map(|claim| claim.monitor.id));
                }
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for worker in workers {
            for id in worker.await.unwrap() {
                assert!(ids.insert(id), "{backend:?} claimed {id} twice");
            }
        }
        assert_eq!(ids.len(), 24);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

fn temporary_path(backend: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "uptimestatus-{backend}-{}-{nonce}.db",
        std::process::id()
    ))
}

#[tokio::test]
async fn incident_diagnostics_survive_retention_on_sqlite_and_turso() {
    for (scheme, backend) in [("sqlite", Backend::Sqlite), ("turso", Backend::Turso)] {
        let path = temporary_path(scheme);
        let store = connect(&format!("{scheme}:{}", path.display()), backend).await;
        store.migrate().await.unwrap();
        let monitor = store
            .create_monitor(&http_spec("incident"), t(0))
            .await
            .unwrap();
        for (at, state, failures) in [(10, MonitorState::Pending, 1), (20, MonitorState::Down, 2)] {
            store
                .record_check(&CheckRecord {
                    monitor_id: monitor.id,
                    scheduled_for: t(at),
                    checked_at: t(at),
                    verdict: Verdict {
                        health: Health::Down,
                        latency: None,
                        status_code: Some(503),
                        reason: Some(uptime_domain::DownReason::UnexpectedStatus(503)),
                        response_body: Some("upstream unavailable".into()),
                    },
                    runtime: Runtime {
                        state,
                        consecutive_failures: failures,
                    },
                    transition: (failures == 2).then_some(Transition::WentDown),
                    cert: None,
                    next_run_at: t(at + 60),
                    region: "test".into(),
                })
                .await
                .unwrap();
        }
        let incident = store.list_incidents(10).await.unwrap().remove(0);
        assert_eq!(
            incident
                .updates
                .iter()
                .filter(|u| u.check.is_some())
                .count(),
            2
        );
        assert_eq!(
            incident.updates[0]
                .check
                .as_ref()
                .unwrap()
                .response_body
                .as_deref(),
            Some("upstream unavailable")
        );
        assert_eq!(
            store.recent_checks(monitor.id, 1).await.unwrap()[0]
                .response_body
                .as_deref(),
            Some("upstream unavailable")
        );
        store.prune_checks_before(t(30), 10).await.unwrap();
        assert_eq!(
            store.incident(incident.id).await.unwrap().unwrap(),
            incident
        );
        let public = store.public_incident(incident.id).await.unwrap().unwrap();
        assert_eq!(public.updates.len(), 1);
        assert!(public.updates[0].check.is_none());
        assert!(store.delete_incident(incident.id).await.unwrap());
        assert!(store.incident(incident.id).await.unwrap().is_none());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
