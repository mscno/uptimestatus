#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Status page subscribers: double opt-in, signed links, fan-out of incidents
//! and maintenance, and webhook failure handling.

mod common;

use std::time::Duration;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{
    ComponentSpec, Health, Impact, IncidentStatus, MaintenanceSpec, MonitorId, MonitorState,
    PageEvent, PageSpec, Runtime, SectionSpec, Selection, SubscriberKind, SubscriberStatus, Theme,
    Transition, Verdict,
};
use uptime_store::{
    CheckRecord, DnsRecord, DomainState, EmailSettingsInput, LinkPurpose, Message, NewIncident,
    NewSubscription, SenderDomain, Store, Subscribed, Subscriber,
};
use uptime_testkit::TestDb;

fn page(slug: &str, monitors: &[&str], published: bool) -> PageSpec {
    PageSpec {
        slug: slug.parse().unwrap(),
        title: format!("{slug} status"),
        description: None,
        accent: None,
        theme: Theme::Auto,
        look: Default::default(),
        published,
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

/// Monitors `api` and `db`; page `platform` shows both, `draft` (unpublished) shows `api`.
async fn setup(db: &TestDb) -> (Store, i64, MonitorId) {
    let store = db.store().clone().with_subscriptions(true);
    let api = store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    store.create_monitor(&tcp_spec("db"), t(0)).await.unwrap();
    let platform = store
        .create_page(&page("platform", &["api", "db"], true))
        .await
        .unwrap();
    store
        .create_page(&page("draft", &["api"], false))
        .await
        .unwrap();
    (store, platform.id, api.id)
}

async fn subscribe_email(store: &Store, page: i64, email: &str, selection: &[&str]) -> Subscriber {
    let outcome = store
        .subscribe(
            page,
            &NewSubscription::Email {
                email: email.parse().unwrap(),
                selection: Selection(selection.iter().map(|s| (*s).to_owned()).collect()),
            },
            t(0),
        )
        .await
        .unwrap();
    let Subscribed::Created(subscriber) = outcome else {
        panic!("expected a new subscription, got {outcome:?}");
    };
    subscriber
}

async fn active(store: &Store, page: i64, email: &str, selection: &[&str]) -> Subscriber {
    let subscriber = subscribe_email(store, page, email, selection).await;
    store
        .confirm_subscriber(subscriber.id, t(1))
        .await
        .unwrap()
        .unwrap()
}

/// Claims everything due and marks it sent; returns `(subscriber id, message)`.
async fn drain(store: &Store, at: i64) -> Vec<(i64, Message)> {
    let mut out = Vec::new();
    for delivery in store
        .claim_subscriber_outbox(t(at), 100, Duration::from_secs(60))
        .await
        .unwrap()
    {
        store
            .mark_subscriber_message_sent(delivery.id, t(at))
            .await
            .unwrap();
        out.push((delivery.subscriber.id, delivery.message));
    }
    out
}

fn types(messages: &[(i64, Message)]) -> Vec<(i64, String)> {
    messages
        .iter()
        .map(|(id, message)| {
            let kind = match message {
                Message::Event(event) => event.type_name().to_owned(),
                Message::Confirm => "confirm".to_owned(),
                Message::Disabled => "disabled".to_owned(),
            };
            (*id, kind)
        })
        .collect()
}

#[tokio::test]
async fn subscriptions_need_confirming_and_links_can_be_revoked() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let pending = subscribe_email(&store, page, "Alice@Example.com", &[]).await;
    assert_eq!(pending.status, SubscriberStatus::Pending);
    assert_eq!(pending.kind, SubscriberKind::Email);
    // The confirmation is the only message until confirmed.
    assert_eq!(
        types(&drain(&store, 2).await),
        [(pending.id, "confirm".into())]
    );

    // Asking again (any case) re-sends the confirmation, never a duplicate row.
    let again = store
        .subscribe(
            page,
            &NewSubscription::Email {
                email: "alice@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(3),
        )
        .await
        .unwrap();
    assert!(matches!(again, Subscribed::Reconfirm(ref s) if s.id == pending.id));
    assert_eq!(store.list_subscribers(page).await.unwrap().len(), 1);
    assert_eq!(
        types(&drain(&store, 4).await),
        [(pending.id, "confirm".into())]
    );

    let token = store
        .subscriber_token(pending.id, LinkPurpose::Confirm, t(100))
        .await
        .unwrap();
    // Wrong purpose, expired, or tampered tokens open nothing.
    assert_eq!(
        store
            .open_subscriber_token(&token, LinkPurpose::Unsubscribe, t(10))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .open_subscriber_token(&token, LinkPurpose::Confirm, t(101))
            .await
            .unwrap(),
        None
    );
    let tampered = format!("{}0", &token[..token.len() - 1]);
    assert_eq!(
        store
            .open_subscriber_token(&tampered, LinkPurpose::Confirm, t(10))
            .await
            .unwrap(),
        None
    );
    let opened = store
        .open_subscriber_token(&token, LinkPurpose::Confirm, t(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened.id, pending.id);
    let confirmed = store
        .confirm_subscriber(opened.id, t(11))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.status, SubscriberStatus::Active);
    // Already active: subscribing again changes nothing and sends nothing.
    let unchanged = store
        .subscribe(
            page,
            &NewSubscription::Email {
                email: "alice@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(12),
        )
        .await
        .unwrap();
    assert!(matches!(unchanged, Subscribed::Unchanged(_)));
    assert!(drain(&store, 13).await.is_empty());

    // Unsubscribing revokes every earlier link.
    let manage = store
        .subscriber_token(pending.id, LinkPurpose::Manage, t(1000))
        .await
        .unwrap();
    store.unsubscribe(pending.id, t(20)).await.unwrap();
    assert_eq!(
        store.subscriber(pending.id).await.unwrap().unwrap().status,
        SubscriberStatus::Unsubscribed
    );
    assert_eq!(
        store
            .open_subscriber_token(&manage, LinkPurpose::Manage, t(21))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn contact_details_are_sealed_and_found_through_blind_indexes() {
    let db = TestDb::new().await;
    let store = db
        .store()
        .clone()
        .with_keyring(
            uptime_store::secrets::Keyring::parse(&uptime_store::secrets::Keyring::generate())
                .unwrap(),
        )
        .with_subscriptions(true);
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let page = store
        .create_page(&page("platform", &["api"], true))
        .await
        .unwrap()
        .id;
    active(&store, page, "secret-person@example.com", &[]).await;
    let hook = store
        .subscribe(
            page,
            &NewSubscription::Webhook {
                url: "https://hooks.example.com/private-path?token=abc"
                    .parse()
                    .unwrap(),
                email: "owner@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(0),
        )
        .await
        .unwrap();
    let Subscribed::Created(hook) = hook else {
        panic!("{hook:?}")
    };
    store.confirm_subscriber(hook.id, t(1)).await.unwrap();
    assert_eq!(store.plaintext_secrets().await.unwrap(), 0);

    let (client, connection) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    let dump: String = client
        .query("SELECT row_to_json(s)::text FROM subscribers s", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    for plain in ["secret-person", "owner@", "private-path", "whsec_"] {
        assert!(!dump.contains(plain), "{plain} stored in plaintext: {dump}");
    }
    // Found again through the index, case-insensitively.
    let again = store
        .subscribe(
            page,
            &NewSubscription::Email {
                email: "SECRET-person@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(5),
        )
        .await
        .unwrap();
    assert!(matches!(again, Subscribed::Unchanged(_)));
    // The webhook secret is revealed exactly once, and rotation reveals a new one.
    let secret = store.reveal_webhook_secret(hook.id).await.unwrap().unwrap();
    assert!(secret.starts_with("whsec_"));
    assert_eq!(store.reveal_webhook_secret(hook.id).await.unwrap(), None);
    store.rotate_webhook_secret(hook.id).await.unwrap();
    let rotated = store.reveal_webhook_secret(hook.id).await.unwrap().unwrap();
    assert_ne!(rotated, secret);
    assert_eq!(store.webhook_secret(hook.id).await.unwrap(), Some(rotated));
}

#[tokio::test]
async fn incidents_reach_subscribers_of_published_pages_who_follow_them() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let draft = store.page_by_slug("draft").await.unwrap().unwrap().id;
    let everything = active(&store, page, "all@example.com", &[]).await;
    let db_only = active(&store, page, "db@example.com", &["db"]).await;
    active(&store, draft, "draft@example.com", &[]).await;
    let pending = subscribe_email(&store, page, "pending@example.com", &[]).await;
    drain(&store, 2).await;

    let incident = store
        .declare_incident(
            &NewIncident {
                title: "API errors".into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                message: "Looking into it.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            t(10),
        )
        .await
        .unwrap();
    let sent = drain(&store, 11).await;
    assert_eq!(types(&sent), [(everything.id, "incident.created".into())]);
    let Message::Event(PageEvent::IncidentCreated(notice)) = &sent[0].1 else {
        panic!("{sent:?}")
    };
    assert_eq!(notice.title, "API errors");
    assert_eq!(notice.message, "Looking into it.");
    assert_eq!(notice.monitors, ["api".parse().unwrap()]);

    store
        .post_incident_update(incident.id, IncidentStatus::Resolved, "Fixed.", t(20))
        .await
        .unwrap();
    assert_eq!(
        types(&drain(&store, 21).await),
        [(everything.id, "incident.resolved".into())]
    );

    // News about `db` reaches both; the pending subscriber hears nothing.
    store
        .declare_incident(
            &NewIncident {
                title: "Slow queries".into(),
                impact: Impact::Minor,
                status: IncidentStatus::Identified,
                message: "Reindexing.".into(),
                monitors: vec!["db".parse().unwrap()],
            },
            t(30),
        )
        .await
        .unwrap();
    let mut got = types(&drain(&store, 31).await);
    got.sort();
    assert_eq!(
        got,
        [
            (everything.id, "incident.created".into()),
            (db_only.id, "incident.created".into())
        ]
    );
    assert!(got.iter().all(|(id, _)| *id != pending.id));

    // Retroactive (already resolved) incidents are not announced.
    store
        .declare_incident(
            &NewIncident {
                title: "Yesterday's blip".into(),
                impact: Impact::Minor,
                status: IncidentStatus::Resolved,
                message: "Resolved.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            t(40),
        )
        .await
        .unwrap();
    assert!(drain(&store, 41).await.is_empty());
}

#[tokio::test]
async fn nothing_is_queued_while_subscriptions_are_disabled() {
    let db = TestDb::new().await;
    let (enabled, page, _) = setup(&db).await;
    active(&enabled, page, "all@example.com", &[]).await;
    drain(&enabled, 2).await;
    let disabled = db.store().clone();
    assert!(!disabled.subscriptions_enabled());
    disabled
        .declare_incident(
            &NewIncident {
                title: "API errors".into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                message: "Looking into it.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            t(10),
        )
        .await
        .unwrap();
    assert!(drain(&enabled, 11).await.is_empty());
}

async fn check(store: &Store, id: MonitorId, at: i64, state: MonitorState, failures: u32) {
    let down = state == MonitorState::Down;
    store
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: t(at),
            checked_at: t(at),
            verdict: Verdict {
                health: if down { Health::Down } else { Health::Up },
                latency: Some(Duration::from_millis(5)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state,
                consecutive_failures: failures,
            },
            transition: down.then_some(Transition::WentDown),
            cert: None,
            next_run_at: t(at + 60),
            region: "test".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn automatic_incidents_are_announced_unless_the_page_opts_out() {
    let db = TestDb::new().await;
    let (store, page, api) = setup(&db).await;
    let subscriber = active(&store, page, "all@example.com", &[]).await;
    drain(&store, 2).await;

    check(&store, api, 10, MonitorState::Down, 2).await;
    check(&store, api, 20, MonitorState::Down, 3).await;
    check(&store, api, 70, MonitorState::Up, 0).await;
    let sent = drain(&store, 71).await;
    assert_eq!(
        types(&sent),
        [
            (subscriber.id, "incident.created".into()),
            (subscriber.id, "incident.resolved".into())
        ]
    );
    let Message::Event(PageEvent::IncidentResolved(notice)) = &sent[1].1 else {
        panic!("{sent:?}")
    };
    assert_eq!(notice.message, "TCP api recovered after 1m.");

    let settings = store.email_settings(page).await.unwrap();
    store
        .save_email_settings(
            page,
            &EmailSettingsInput {
                subscriptions: true,
                notify_automatic: false,
                sender_name: settings.sender_name,
                sender_address: settings.sender_address,
                reply_to: settings.reply_to,
            },
        )
        .await
        .unwrap();
    check(&store, api, 100, MonitorState::Down, 2).await;
    check(&store, api, 160, MonitorState::Up, 0).await;
    assert!(drain(&store, 161).await.is_empty());
}

#[tokio::test]
async fn maintenance_is_announced_when_scheduled_started_and_completed_once() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let subscriber = active(&store, page, "all@example.com", &[]).await;
    drain(&store, 2).await;
    store
        .create_maintenance(
            &MaintenanceSpec {
                title: "Database upgrade".into(),
                description: Some("Expect brief read-only periods.".into()),
                starts_at: t(1000),
                ends_at: t(2000),
                repeat: None,
                repeat_until: None,
                monitors: vec!["db".parse().unwrap()],
            },
            t(10),
        )
        .await
        .unwrap();
    assert_eq!(
        types(&drain(&store, 11).await),
        [(subscriber.id, "maintenance.scheduled".into())]
    );
    assert_eq!(store.announce_maintenance(t(900), t(999)).await.unwrap(), 0);
    assert_eq!(
        store.announce_maintenance(t(999), t(1060)).await.unwrap(),
        1
    );
    // A second pass over the same moment queues nothing new.
    assert_eq!(
        store.announce_maintenance(t(950), t(1060)).await.unwrap(),
        0
    );
    assert_eq!(
        store.announce_maintenance(t(1990), t(2010)).await.unwrap(),
        1
    );
    assert_eq!(
        types(&drain(&store, 2011).await),
        [
            (subscriber.id, "maintenance.started".into()),
            (subscriber.id, "maintenance.completed".into())
        ]
    );
}

#[tokio::test]
async fn deleting_an_incident_drops_its_unsent_news() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    active(&store, page, "all@example.com", &[]).await;
    drain(&store, 2).await;
    let incident = store
        .declare_incident(
            &NewIncident {
                title: "Oops".into(),
                impact: Impact::Minor,
                status: IncidentStatus::Investigating,
                message: "Wrong page.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            t(10),
        )
        .await
        .unwrap();
    store.delete_incident(incident.id).await.unwrap();
    assert!(drain(&store, 11).await.is_empty());
}

#[tokio::test]
async fn failing_webhooks_are_disabled_after_ten_failures_over_a_day() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let Subscribed::Created(hook) = store
        .subscribe(
            page,
            &NewSubscription::Webhook {
                url: "https://hooks.example.com/status".parse().unwrap(),
                email: "owner@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(0),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    store.confirm_subscriber(hook.id, t(1)).await.unwrap();
    drain(&store, 2).await;
    // Ten failures within an hour: not yet (they must span a day).
    for i in 0..10 {
        let disabled = store
            .record_webhook_failure(hook.id, "answered 500", t(10 + i * 60))
            .await
            .unwrap();
        assert!(!disabled);
    }
    // A success resets the streak.
    store.record_webhook_success(hook.id, t(700)).await.unwrap();
    assert_eq!(
        store.subscriber(hook.id).await.unwrap().unwrap().failures,
        0
    );
    let day = 24 * 3600;
    let mut disabled = false;
    for i in 0..10 {
        disabled = store
            .record_webhook_failure(hook.id, "answered 500", t(1000 + i * day / 9))
            .await
            .unwrap();
    }
    assert!(disabled);
    let after = store.subscriber(hook.id).await.unwrap().unwrap();
    assert_eq!(after.status, SubscriberStatus::Disabled);
    assert_eq!(after.last_error.as_deref(), Some("answered 500"));
    assert_eq!(
        types(&drain(&store, 1000 + day + 1).await),
        [(hook.id, "disabled".into())]
    );
}

#[tokio::test]
async fn bounces_suppress_an_address_on_every_page() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let other = store.create_page(&page_spec_other()).await.unwrap().id;
    let a = active(&store, page, "bouncy@example.com", &[]).await;
    let b = active(&store, other, "Bouncy@example.com", &[]).await;
    active(&store, page, "fine@example.com", &[]).await;
    let changed = store
        .suppress_email(&"bouncy@example.com".parse().unwrap(), t(5))
        .await
        .unwrap();
    let mut ids: Vec<i64> = changed.iter().map(|s| s.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [a.id, b.id]);
    // Suppressed addresses cannot resubscribe.
    let again = store
        .subscribe(
            page,
            &NewSubscription::Email {
                email: "bouncy@example.com".parse().unwrap(),
                selection: Selection::all(),
            },
            t(6),
        )
        .await
        .unwrap();
    assert!(
        matches!(again, Subscribed::Unchanged(ref s) if s.status == SubscriberStatus::Suppressed)
    );
    let counts = store.subscriber_counts(page).await.unwrap();
    assert_eq!((counts.active, counts.pending, counts.inactive), (1, 0, 1));
}

fn page_spec_other() -> PageSpec {
    page("other", &["api"], true)
}

#[tokio::test]
async fn email_settings_keep_the_sending_domain_only_for_its_sender() {
    let db = TestDb::new().await;
    let (store, page, _) = setup(&db).await;
    let defaults = store.email_settings(page).await.unwrap();
    assert!(defaults.subscriptions && defaults.notify_automatic);
    assert_eq!(defaults.verified_sender(), None);
    let input = EmailSettingsInput {
        subscriptions: true,
        notify_automatic: true,
        sender_name: Some("  Acme Status ".into()),
        sender_address: Some("status@mail.acme.com".parse().unwrap()),
        reply_to: Some("support@acme.com".parse().unwrap()),
    };
    store.save_email_settings(page, &input).await.unwrap();
    let domain = SenderDomain {
        id: "dom_1".into(),
        name: "mail.acme.com".into(),
        status: DomainState::Verified,
        records: vec![DnsRecord {
            kind: "CNAME".into(),
            name: "lm1._domainkey.mail.acme.com".into(),
            value: "lm1.dkim.lettermint.co".into(),
            purpose: "dkim_primary".into(),
            status: "active".into(),
            required: true,
        }],
        checked_at: Some(t(5)),
    };
    let settings = store.set_sender_domain(page, Some(&domain)).await.unwrap();
    assert_eq!(settings.sender_name.as_deref(), Some("Acme Status"));
    assert_eq!(settings.domain.as_ref(), Some(&domain));
    assert_eq!(
        settings
            .verified_sender()
            .map(ToString::to_string)
            .as_deref(),
        Some("status@mail.acme.com")
    );
    // Another sender domain forgets the old registration.
    let moved = store
        .save_email_settings(
            page,
            &EmailSettingsInput {
                sender_address: Some("status@acme.org".parse().unwrap()),
                ..input
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.domain, None);
    assert_eq!(moved.verified_sender(), None);
    // Settings go with their page.
    assert!(store.delete_page(page).await.unwrap());
    assert!(
        store
            .email_settings(page)
            .await
            .unwrap()
            .sender_address
            .is_none()
    );
}

#[tokio::test]
async fn component_status_changes_reach_subscribers_who_follow_the_component() {
    let db = TestDb::new().await;
    let store = db.store().clone().with_subscriptions(true);
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let mut spec = page("billing", &["api"], true);
    spec.sections[0].components.push(ComponentSpec::manual(
        "payments".parse().unwrap(),
        Some("Payments".into()),
        None,
    ));
    let page = store.create_page(&spec).await.unwrap().id;
    let everything = active(&store, page, "all@example.com", &[]).await;
    let payments = active(&store, page, "pay@example.com", &["payments"]).await;
    let api_only = active(&store, page, "api@example.com", &["api"]).await;
    drain(&store, 2).await;

    store
        .set_component_status(
            page,
            "payments",
            Some(uptime_domain::ComponentStatus::Degraded),
            Some("Card payments are slow."),
            "alice",
            t(10),
        )
        .await
        .unwrap();
    let mut got = types(&drain(&store, 11).await);
    got.sort();
    assert_eq!(
        got,
        [
            (everything.id, "component.status_changed".into()),
            (payments.id, "component.status_changed".into())
        ]
    );
    // An override of a monitor component reaches its followers; clearing it is quiet.
    store
        .set_component_status(
            page,
            "api",
            Some(uptime_domain::ComponentStatus::MajorOutage),
            None,
            "alice",
            t(20),
        )
        .await
        .unwrap();
    let sent = drain(&store, 21).await;
    let mut ids: Vec<i64> = sent.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [everything.id, api_only.id]);
    let Message::Event(PageEvent::ComponentStatusChanged(notice)) = &sent[0].1 else {
        panic!("{sent:?}")
    };
    assert_eq!(notice.name, "TCP api");
    assert_eq!(notice.status, "major_outage");
    store
        .set_component_status(page, "api", None, None, "alice", t(30))
        .await
        .unwrap();
    assert!(drain(&store, 31).await.is_empty());
}
