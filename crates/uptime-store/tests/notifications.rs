#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use std::time::Duration;

use common::{http_spec, t};
use pretty_assertions::assert_eq;
use uptime_domain::{
    AlertEvent, ChannelKind, ChannelSpec, DownReason, FailureKind, Health, MonitorId, MonitorState,
    Runtime, Transition, Verdict,
};
use uptime_store::{CheckRecord, DeliveryStatus, Store, StoreError};
use uptime_testkit::TestDb;

fn channel(name: &str, kind: ChannelKind) -> ChannelSpec {
    let url = match kind {
        ChannelKind::Slack => "https://hooks.slack.com/services/T/B/x",
        ChannelKind::Discord => "https://discord.com/api/webhooks/1/x",
        ChannelKind::Webhook => "https://ops.example.com/hook",
    };
    ChannelSpec {
        name: name.into(),
        kind,
        url: url.parse().unwrap(),
        secret: (kind == ChannelKind::Webhook).then(|| "s3cret".to_owned()),
        default_on: false,
        routing: Default::default(),
    }
}

async fn record(
    store: &Store,
    id: MonitorId,
    at: i64,
    state: MonitorState,
    transition: Option<Transition>,
) {
    let down = state == MonitorState::Down;
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if down { Health::Down } else { Health::Up },
                latency: Some(Duration::from_millis(80)),
                status_code: None,
                reason: down.then(|| DownReason::Probe {
                    kind: FailureKind::Refused,
                    message: "connection refused".into(),
                }),
            },
            runtime: Runtime {
                state,
                consecutive_failures: u32::from(down),
            },
            transition,
            cert: None,
            next_run_at: t(at + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn channels_are_created_listed_updated_and_deleted() {
    let db = TestDb::new().await;
    let store = db.store();
    let ops = store
        .create_channel(&channel("Ops", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    store
        .create_channel(&channel("Alerts", ChannelKind::Webhook), t(0))
        .await
        .unwrap();

    let names: Vec<_> = store
        .list_channels()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.spec.name)
        .collect();
    assert_eq!(names, ["Alerts", "Ops"]);

    let renamed = ChannelSpec {
        name: "Ops team".into(),
        default_on: true,
        routing: Default::default(),
        ..channel("Ops", ChannelKind::Slack)
    };
    store.update_channel(ops.id, &renamed).await.unwrap();
    assert_eq!(store.channel(ops.id).await.unwrap().unwrap().spec, renamed);
    assert_eq!(store.default_channels().await.unwrap(), [ops.id]);

    assert!(store.delete_channel(ops.id).await.unwrap());
    assert!(!store.delete_channel(ops.id).await.unwrap());
    assert!(matches!(
        store.update_channel(ops.id, &renamed).await,
        Err(StoreError::ChannelNotFound(_))
    ));
}

#[tokio::test]
async fn monitors_choose_their_channels() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    let b = store
        .create_channel(&channel("B", ChannelKind::Discord), t(0))
        .await
        .unwrap();

    store
        .set_monitor_channels(monitor.id, &[b.id, a.id])
        .await
        .unwrap();
    assert_eq!(
        store.monitor_channels(monitor.id).await.unwrap(),
        [a.id, b.id]
    );

    store
        .set_monitor_channels(monitor.id, &[b.id])
        .await
        .unwrap();
    assert_eq!(store.monitor_channels(monitor.id).await.unwrap(), [b.id]);

    assert!(matches!(
        store.set_monitor_channels(monitor.id, &[4242]).await,
        Err(StoreError::ChannelNotFound(4242))
    ));

    store.delete_channel(b.id).await.unwrap();
    assert_eq!(
        store.monitor_channels(monitor.id).await.unwrap(),
        Vec::<i64>::new()
    );
}

#[tokio::test]
async fn transitions_queue_one_alert_per_attached_channel() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let other = store.create_monitor(&http_spec("web"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    let b = store
        .create_channel(&channel("B", ChannelKind::Webhook), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id, b.id])
        .await
        .unwrap();

    record(store, monitor.id, 0, MonitorState::Pending, None).await;
    record(
        store,
        other.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    assert!(
        store
            .claim_outbox(t(1), 10, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );

    record(
        store,
        monitor.id,
        20,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;

    let mut items = store
        .claim_outbox(t(21), 10, Duration::from_secs(60))
        .await
        .unwrap();
    items.sort_by_key(|item| item.channel.id);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].channel.id, a.id);
    assert_eq!(items[1].channel.spec.secret.as_deref(), Some("s3cret"));
    let alert = &items[0].alert;
    assert_eq!(alert.event, AlertEvent::WentDown);
    assert_eq!(alert.monitor.key, "api");
    assert_eq!(alert.monitor.name, "Monitor api");
    assert_eq!(
        (alert.previous_state, alert.state),
        (MonitorState::Pending, MonitorState::Down)
    );
    assert_eq!(alert.error.as_deref(), Some("refused: connection refused"));
    assert_eq!(alert.at, t(20));
    assert_eq!(items[0].attempts, 1);
}

#[tokio::test]
async fn recoveries_report_how_long_the_monitor_was_down() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id])
        .await
        .unwrap();
    record(store, monitor.id, 100, MonitorState::Down, None).await;
    record(store, monitor.id, 160, MonitorState::Down, None).await;

    record(
        store,
        monitor.id,
        400,
        MonitorState::Up,
        Some(Transition::Recovered),
    )
    .await;

    let items = store
        .claim_outbox(t(401), 10, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(items[0].alert.event, AlertEvent::Recovered);
    assert_eq!(items[0].alert.downtime_secs, Some(300));
    assert_eq!(items[0].alert.error, None);
}

#[tokio::test]
async fn deliveries_are_leased_retried_and_finished() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id])
        .await
        .unwrap();
    record(
        store,
        monitor.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    let lease = Duration::from_secs(60);

    let first = store.claim_outbox(t(1), 10, lease).await.unwrap();
    assert_eq!(first.len(), 1);
    assert!(
        store
            .claim_outbox(t(2), 10, lease)
            .await
            .unwrap()
            .is_empty(),
        "leased"
    );
    let again = store.claim_outbox(t(62), 10, lease).await.unwrap();
    assert_eq!(again[0].attempts, 2, "an expired lease is claimable again");

    store
        .mark_delivery_failed(again[0].id, "502 Bad Gateway", Some(t(300)))
        .await
        .unwrap();
    assert!(
        store
            .claim_outbox(t(299), 10, lease)
            .await
            .unwrap()
            .is_empty()
    );
    let retry = store.claim_outbox(t(300), 10, lease).await.unwrap();
    assert_eq!(retry[0].attempts, 3);

    store.mark_delivered(retry[0].id, t(301)).await.unwrap();
    assert!(
        store
            .claim_outbox(t(10_000), 10, lease)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deliveries_that_give_up_are_never_retried() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id])
        .await
        .unwrap();
    record(
        store,
        monitor.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    let item = store
        .claim_outbox(t(1), 10, Duration::from_secs(60))
        .await
        .unwrap()
        .remove(0);

    store
        .mark_delivery_failed(item.id, "404 no_service", None)
        .await
        .unwrap();

    assert!(
        store
            .claim_outbox(t(100_000), 10, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
    let recent = store.recent_deliveries(10).await.unwrap();
    assert_eq!(recent[0].status, DeliveryStatus::Failed);
    assert_eq!(recent[0].last_error.as_deref(), Some("404 no_service"));
    assert_eq!(recent[0].channel_name, "A");
    assert_eq!(recent[0].monitor_name.as_deref(), Some("Monitor api"));
}

#[tokio::test]
async fn concurrent_claims_never_share_a_delivery() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let mut channels = Vec::new();
    for i in 0..10 {
        channels.push(
            store
                .create_channel(&channel(&format!("C{i}"), ChannelKind::Webhook), t(0))
                .await
                .unwrap()
                .id,
        );
    }
    store
        .set_monitor_channels(monitor.id, &channels)
        .await
        .unwrap();
    record(
        store,
        monitor.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;

    let claims = futures_util::future::join_all(
        (0..4).map(|_| store.claim_outbox(t(1), 3, Duration::from_secs(60))),
    )
    .await;

    let mut ids: Vec<i64> = claims
        .into_iter()
        .flat_map(|claim| claim.unwrap().into_iter().map(|item| item.id))
        .collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "no delivery claimed twice");
    assert_eq!(total, 10);
}

#[tokio::test]
async fn deleting_a_channel_drops_its_pending_deliveries() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id])
        .await
        .unwrap();
    record(
        store,
        monitor.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;

    store.delete_channel(a.id).await.unwrap();

    assert!(
        store
            .claim_outbox(t(1), 10, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn old_finished_deliveries_are_pruned_but_pending_ones_kept() {
    let db = TestDb::new().await;
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let a = store
        .create_channel(&channel("A", ChannelKind::Slack), t(0))
        .await
        .unwrap();
    let b = store
        .create_channel(&channel("B", ChannelKind::Webhook), t(0))
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[a.id, b.id])
        .await
        .unwrap();
    record(
        store,
        monitor.id,
        0,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    let claimed = store
        .claim_outbox(t(1), 10, Duration::from_secs(60))
        .await
        .unwrap();
    store.mark_delivered(claimed[0].id, t(2)).await.unwrap();

    let pruned = store.prune_deliveries_before(t(100)).await.unwrap();

    assert_eq!(pruned, 1, "only the delivered one");
    let left = store.recent_deliveries(10).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].status, DeliveryStatus::Pending);
}

/// One monitor alerting one channel with `routing`.
async fn routed(db: &TestDb, routing: uptime_domain::Routing) -> (MonitorId, i64) {
    let store = db.store();
    let monitor = store.create_monitor(&http_spec("api"), t(0)).await.unwrap();
    let channel = store
        .create_channel(
            &ChannelSpec {
                routing,
                ..channel("Pager", ChannelKind::Webhook)
            },
            t(0),
        )
        .await
        .unwrap();
    store
        .set_monitor_channels(monitor.id, &[channel.id])
        .await
        .unwrap();
    (monitor.id, channel.id)
}

async fn claimed_events(store: &Store, at: i64) -> Vec<AlertEvent> {
    store
        .claim_outbox(t(at), 10, Duration::from_secs(60))
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.alert.event)
        .collect()
}

#[tokio::test]
async fn routing_is_saved_with_the_channel() {
    let db = TestDb::new().await;
    let routing = uptime_domain::Routing {
        escalate_after_mins: 15,
        quiet: Some(uptime_domain::QuietHours {
            start_min: 22 * 60,
            end_min: 6 * 60,
        }),
        mute: vec![AlertEvent::Resend],
    };
    let created = db
        .store()
        .create_channel(
            &ChannelSpec {
                routing: routing.clone(),
                ..channel("Pager", ChannelKind::Slack)
            },
            t(0),
        )
        .await
        .unwrap();
    assert_eq!(created.spec.routing, routing);
    assert_eq!(
        db.store()
            .channel(created.id)
            .await
            .unwrap()
            .unwrap()
            .spec
            .routing,
        routing
    );
}

#[tokio::test]
async fn an_escalating_channel_waits_and_drops_outages_that_recover_first() {
    let db = TestDb::new().await;
    let store = db.store();
    let (monitor, _) = routed(
        &db,
        uptime_domain::Routing {
            escalate_after_mins: 10,
            ..Default::default()
        },
    )
    .await;

    record(
        store,
        monitor,
        100,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    assert!(claimed_events(store, 101).await.is_empty(), "held back");

    // Back up after five minutes: neither the outage nor the recovery is announced.
    record(
        store,
        monitor,
        400,
        MonitorState::Up,
        Some(Transition::Recovered),
    )
    .await;
    assert!(claimed_events(store, 100 + 3600).await.is_empty());
}

#[tokio::test]
async fn an_escalating_channel_hears_about_long_outages_and_their_recovery() {
    let db = TestDb::new().await;
    let store = db.store();
    let (monitor, channel) = routed(
        &db,
        uptime_domain::Routing {
            escalate_after_mins: 10,
            ..Default::default()
        },
    )
    .await;

    record(
        store,
        monitor,
        100,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    assert!(claimed_events(store, 300).await.is_empty());
    let due = store
        .claim_outbox(t(100 + 600), 10, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].alert.event, AlertEvent::WentDown);
    assert_eq!(due[0].channel.id, channel);
    store.mark_delivered(due[0].id, t(700)).await.unwrap();

    record(
        store,
        monitor,
        1000,
        MonitorState::Up,
        Some(Transition::Recovered),
    )
    .await;
    assert_eq!(claimed_events(store, 1001).await, [AlertEvent::Recovered]);
}

#[tokio::test]
async fn quiet_hours_hold_alerts_until_they_end_and_muted_events_vanish() {
    let db = TestDb::new().await;
    let store = db.store();
    // `t(n)` is n seconds after 2027-01-15T08:00:00Z: quiet 08:00–09:00 UTC.
    let midnight = t(0).as_second().rem_euclid(86_400);
    let start_min = u16::try_from(midnight / 60).unwrap();
    let (monitor, _) = routed(
        &db,
        uptime_domain::Routing {
            quiet: Some(uptime_domain::QuietHours {
                start_min,
                end_min: (start_min + 60) % 1440,
            }),
            mute: vec![AlertEvent::Recovered],
            ..Default::default()
        },
    )
    .await;

    record(
        store,
        monitor,
        600,
        MonitorState::Down,
        Some(Transition::WentDown),
    )
    .await;
    assert!(claimed_events(store, 601).await.is_empty(), "quiet");
    let due = store
        .claim_outbox(t(3600), 10, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].alert.event, AlertEvent::WentDown);
    store.mark_delivered(due[0].id, t(3601)).await.unwrap();

    record(
        store,
        monitor,
        4000,
        MonitorState::Up,
        Some(Transition::Recovered),
    )
    .await;
    assert!(
        claimed_events(store, 9000).await.is_empty(),
        "recoveries muted"
    );
}
