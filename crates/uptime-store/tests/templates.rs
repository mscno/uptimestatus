#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Incident templates and scheduled incident updates.

mod common;

use common::{t, tcp_spec};
use pretty_assertions::assert_eq;
use uptime_domain::{Impact, IncidentStatus, IncidentTemplate};
use uptime_store::{NewIncident, NewScheduledUpdate, ScheduledKind, StoreError};
use uptime_testkit::TestDb;

fn template(name: &str) -> IncidentTemplate {
    IncidentTemplate {
        name: name.into(),
        title: "{monitor} is degraded".into(),
        impact: Impact::Minor,
        status: IncidentStatus::Investigating,
        body: "We are looking into slow responses from {monitors}.".into(),
    }
}

#[tokio::test]
async fn templates_are_created_listed_updated_and_deleted() {
    let db = TestDb::new().await;
    let store = db.store();
    let slow = store.create_template(&template("  Slow ")).await.unwrap();
    assert_eq!(slow.spec.name, "Slow", "trimmed");
    store.create_template(&template("Down")).await.unwrap();
    let names: Vec<String> = store
        .list_templates()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.spec.name)
        .collect();
    assert_eq!(names, ["Down", "Slow"], "sorted by name");

    assert!(matches!(
        store.create_template(&template("Down")).await,
        Err(StoreError::DuplicateTemplate(name)) if name == "Down"
    ));
    assert!(matches!(
        store
            .create_template(&IncidentTemplate {
                title: String::new(),
                ..template("Empty")
            })
            .await,
        Err(StoreError::InvalidTemplate(error)) if error.field == "title"
    ));

    let renamed = store
        .update_template(
            slow.id,
            &IncidentTemplate {
                impact: Impact::Major,
                ..template("Very slow")
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.spec.impact, Impact::Major);
    assert_eq!(store.template(slow.id).await.unwrap(), Some(renamed));
    assert!(matches!(
        store.update_template(slow.id, &template("Down")).await,
        Err(StoreError::DuplicateTemplate(_))
    ));
    assert!(matches!(
        store.update_template(999, &template("Gone")).await,
        Err(StoreError::TemplateNotFound(999))
    ));

    assert!(store.delete_template(slow.id).await.unwrap());
    assert!(!store.delete_template(slow.id).await.unwrap());
    assert_eq!(store.list_templates().await.unwrap().len(), 1);
}

fn declared(monitors: &[&str]) -> NewIncident {
    NewIncident {
        title: "Database maintenance overran".into(),
        impact: Impact::Major,
        status: IncidentStatus::Identified,
        message: "We are finishing the migration.".into(),
        monitors: monitors.iter().map(|k| k.parse().unwrap()).collect(),
    }
}

#[tokio::test]
async fn scheduled_updates_are_listed_due_and_published_once() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let incident = store
        .declare_incident(&declared(&["api"]), t(10))
        .await
        .unwrap();

    let resolve = store
        .schedule_update(&NewScheduledUpdate {
            publish_at: t(600),
            status: IncidentStatus::Resolved,
            body: "All clear.".into(),
            kind: ScheduledKind::Update {
                incident_id: incident.id,
            },
            actor: "alice".into(),
        })
        .await
        .unwrap();
    let monitoring = store
        .schedule_update(&NewScheduledUpdate {
            publish_at: t(300),
            status: IncidentStatus::Monitoring,
            body: "Watching closely.".into(),
            kind: ScheduledKind::Update {
                incident_id: incident.id,
            },
            actor: "alice".into(),
        })
        .await
        .unwrap();
    let ids: Vec<i64> = store
        .scheduled_for_incident(incident.id)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, [monitoring.id, resolve.id], "soonest first");
    assert!(store.due_scheduled(t(299), 10).await.unwrap().is_empty());
    let due = store.due_scheduled(t(300), 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0], monitoring);

    // Publishing removes the schedule in the same transaction, so a second
    // publisher finds nothing to do.
    let published = store
        .post_scheduled_update(&monitoring, t(301))
        .await
        .unwrap();
    assert_eq!(published.status, IncidentStatus::Monitoring);
    assert_eq!(published.updates[0].body, "Watching closely.");
    assert_eq!(published.updates[0].created_at, t(301));
    assert!(matches!(
        store.post_scheduled_update(&monitoring, t(302)).await,
        Err(StoreError::ScheduledNotFound(id)) if id == monitoring.id
    ));
    assert_eq!(
        store
            .incident(incident.id)
            .await
            .unwrap()
            .unwrap()
            .updates
            .len(),
        2
    );

    // Rescheduling moves it and clears a failure; cancelling removes it.
    store.fail_scheduled(resolve.id, "boom").await.unwrap();
    assert!(
        store.due_scheduled(t(9999), 10).await.unwrap().is_empty(),
        "failed ones wait"
    );
    let moved = store
        .reschedule(
            resolve.id,
            t(900),
            IncidentStatus::Resolved,
            "Fixed for good.",
            None,
        )
        .await
        .unwrap();
    assert_eq!((moved.publish_at, moved.error.as_deref()), (t(900), None));
    assert_eq!(moved.body, "Fixed for good.");
    assert!(store.cancel_scheduled(resolve.id).await.unwrap());
    assert!(!store.cancel_scheduled(resolve.id).await.unwrap());
    assert!(
        store
            .scheduled_for_incident(incident.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn scheduled_declarations_open_incidents_when_published() {
    let db = TestDb::new().await;
    let store = db.store();
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
    let new = declared(&["api"]);
    let scheduled = store
        .schedule_update(&NewScheduledUpdate {
            publish_at: t(1000),
            status: new.status,
            body: new.message.clone(),
            kind: ScheduledKind::Declare {
                title: new.title.clone(),
                impact: new.impact,
                monitors: new.monitors.clone(),
            },
            actor: "alice".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        store.scheduled_declarations().await.unwrap(),
        std::slice::from_ref(&scheduled)
    );
    assert!(
        store.list_incidents(10).await.unwrap().is_empty(),
        "nothing public yet"
    );

    let incident = store
        .declare_scheduled_incident(&scheduled, t(1000))
        .await
        .unwrap();
    assert_eq!(incident.title, new.title);
    assert_eq!(incident.started_at, t(1000));
    assert_eq!(incident.monitor_ids.len(), 1);
    assert!(store.scheduled_declarations().await.unwrap().is_empty());

    // Unknown monitors are refused when scheduling.
    assert!(matches!(
        store
            .schedule_update(&NewScheduledUpdate {
                kind: ScheduledKind::Declare {
                    title: "x".into(),
                    impact: Impact::Minor,
                    monitors: vec!["nope".parse().unwrap()],
                },
                ..scheduled_update(incident.id)
            })
            .await,
        Err(StoreError::UnknownMonitors(_))
    ));
    // Updates need an incident; deleting the incident drops its schedule.
    assert!(matches!(
        store.schedule_update(&scheduled_update(999)).await,
        Err(StoreError::IncidentNotFound(999))
    ));
    store
        .schedule_update(&scheduled_update(incident.id))
        .await
        .unwrap();
    assert!(store.delete_incident(incident.id).await.unwrap());
    assert!(store.due_scheduled(t(99_999), 10).await.unwrap().is_empty());
}

fn scheduled_update(incident_id: i64) -> NewScheduledUpdate {
    NewScheduledUpdate {
        publish_at: t(2000),
        status: IncidentStatus::Resolved,
        body: "Resolved.".into(),
        kind: ScheduledKind::Update { incident_id },
        actor: "alice".into(),
    }
}

#[tokio::test]
async fn subscribers_hear_of_scheduled_items_only_when_published() {
    use std::time::Duration;

    use uptime_domain::{ComponentSpec, PageSpec, SectionSpec, Selection, Theme};
    use uptime_store::{Message, NewSubscription, Subscribed};

    let db = TestDb::new().await;
    let store = db.store().clone().with_subscriptions(true);
    store.create_monitor(&tcp_spec("api"), t(0)).await.unwrap();
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
    let Subscribed::Created(subscriber) = store
        .subscribe(
            page.id,
            &NewSubscription::Email {
                email: "alice@example.com".parse().unwrap(),
                selection: Selection(Vec::new()),
            },
            t(0),
        )
        .await
        .unwrap()
    else {
        panic!("expected a new subscription");
    };
    store.confirm_subscriber(subscriber.id, t(1)).await.unwrap();
    // Events queued for the subscriber, oldest first (confirmation aside).
    let drain = |at: i64| {
        let store = store.clone();
        async move {
            let mut events = Vec::new();
            for delivery in store
                .claim_subscriber_outbox(t(at), 100, Duration::from_secs(60))
                .await
                .unwrap()
            {
                store
                    .mark_subscriber_message_sent(delivery.id, t(at))
                    .await
                    .unwrap();
                if let Message::Event(event) = delivery.message {
                    events.push(event.type_name().to_owned());
                }
            }
            events
        }
    };
    drain(2).await;

    let incident = store
        .declare_incident(&declared(&["api"]), t(10))
        .await
        .unwrap();
    assert_eq!(drain(11).await.len(), 1, "the declaration");
    let declaration = store
        .schedule_update(&NewScheduledUpdate {
            kind: ScheduledKind::Declare {
                title: "Planned work".into(),
                impact: Impact::Minor,
                monitors: vec!["api".parse().unwrap()],
            },
            status: IncidentStatus::Identified,
            ..scheduled_update(incident.id)
        })
        .await
        .unwrap();
    let resolution = store
        .schedule_update(&scheduled_update(incident.id))
        .await
        .unwrap();
    assert!(drain(20).await.is_empty(), "scheduling is not news");

    store
        .post_scheduled_update(&resolution, t(2000))
        .await
        .unwrap();
    store
        .declare_scheduled_incident(&declaration, t(2000))
        .await
        .unwrap();
    assert_eq!(drain(2001).await.len(), 2, "each publication, once");
}
