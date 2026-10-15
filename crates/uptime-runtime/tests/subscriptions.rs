#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Delivering subscribers' messages: branded email through a mock Lettermint,
//! signed webhooks, retries and disabling.

use std::sync::{Arc, Mutex};

use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use uptime_domain::{
    ComponentSpec, DomainState, Impact, IncidentStatus, PageSpec, SectionSpec, Selection,
    SenderDomain, SubscriberStatus, TcpCheck, Theme,
};
use uptime_notify::{Mailer, MailerConfig, Sender, signature};
use uptime_runtime::{Courier, NotifierConfig};
use uptime_store::{
    EmailSettingsInput, NewIncident, NewSubscription, Store, Subscribed, Subscriber,
};
use uptime_testkit::TestDb;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

fn base() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

#[derive(Clone)]
struct Clock(Arc<Mutex<Timestamp>>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(base())))
    }

    fn set(&self, at: Timestamp) {
        *self.0.lock().unwrap() = at;
    }

    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }

    fn function(&self) -> uptime_runtime::Clock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }
}

struct World {
    _db: TestDb,
    store: Store,
    lettermint: MockServer,
    hooks: MockServer,
    clock: Clock,
    page: i64,
}

impl World {
    async fn new() -> Self {
        let db = TestDb::new().await;
        let store = db.store().clone().with_subscriptions(true);
        let mut spec = uptime_domain::MonitorSpec {
            key: "api".parse().unwrap(),
            name: "API service".into(),
            check: uptime_domain::CheckSpec::Tcp(TcpCheck::connect("api.example.com", 443)),
            policy: Default::default(),
            active: true,
            tags: Vec::new(),
            group: None,
        };
        store.create_monitor(&spec, base()).await.unwrap();
        spec.key = "db".parse().unwrap();
        spec.name = "Database".into();
        store.create_monitor(&spec, base()).await.unwrap();
        let page = store
            .create_page(&PageSpec {
                slug: "acme".parse().unwrap(),
                title: "Acme Status".into(),
                description: None,
                accent: Some("#ff3366".parse().unwrap()),
                theme: Theme::Auto,
                look: uptime_domain::Look::Clean,
                published: true,
                website: None,
                custom_css: None,
                sections: vec![SectionSpec {
                    name: "Core".into(),
                    components: vec![
                        ComponentSpec::of_monitor(
                            "api".parse().unwrap(),
                            Some("Public API".into()),
                        ),
                        ComponentSpec::of_monitor("db".parse().unwrap(), None),
                    ],
                }],
            })
            .await
            .unwrap()
            .id;
        let lettermint = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/send"))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_json(json!({ "message_id": "m", "status": "pending" })),
            )
            .mount(&lettermint)
            .await;
        Self {
            _db: db,
            store,
            lettermint,
            hooks: MockServer::start().await,
            clock: Clock::new(),
            page,
        }
    }

    fn courier(&self) -> Courier {
        let mailer = Mailer::new(MailerConfig {
            project_token: "lm_project_test".into(),
            team_token: None,
            default_from: "uptimestatus <alerts@parap.ly>".parse().unwrap(),
            api_url: Some(self.lettermint.uri()),
        })
        .unwrap();
        // Unguarded: the webhook receiver listens on localhost.
        let sender = Sender::new(reqwest::Client::new()).with_mailer(mailer);
        Courier::new(
            self.store.clone(),
            sender,
            "https://console.example.com".parse().unwrap(),
            NotifierConfig::default(),
        )
        .with_clock(self.clock.function())
    }

    async fn emails(&self) -> Vec<(Value, Request)> {
        self.lettermint
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|r| (serde_json::from_slice(&r.body).unwrap(), r))
            .collect()
    }

    async fn subscribe(&self, request: NewSubscription) -> Subscriber {
        match self
            .store
            .subscribe(self.page, &request, self.clock.now())
            .await
            .unwrap()
        {
            Subscribed::Created(s) => s,
            other => panic!("{other:?}"),
        }
    }

    async fn declare(&self, title: &str, monitor: &str) {
        self.store
            .declare_incident(
                &NewIncident {
                    title: title.into(),
                    impact: Impact::Major,
                    status: IncidentStatus::Investigating,
                    message: "We are looking into it.".into(),
                    monitors: vec![monitor.parse().unwrap()],
                },
                self.clock.now(),
            )
            .await
            .unwrap();
    }
}

fn link_in(text: &str, path: &str) -> String {
    let start = text
        .find(path)
        .unwrap_or_else(|| panic!("{path} in {text}"));
    let from = text[..start].rfind("https://").unwrap();
    text[from..]
        .split_whitespace()
        .next()
        .unwrap()
        .trim_end_matches(['"', '>'])
        .to_owned()
}

#[tokio::test]
async fn email_subscribers_get_a_confirmation_then_branded_news() {
    let world = World::new().await;
    world
        .store
        .save_email_settings(
            world.page,
            &EmailSettingsInput {
                subscriptions: true,
                notify_automatic: true,
                sender_name: Some("Acme".into()),
                sender_address: Some("status@mail.acme.com".parse().unwrap()),
                reply_to: None,
            },
        )
        .await
        .unwrap();
    let reader = world
        .subscribe(NewSubscription::Email {
            email: "reader@example.com".parse().unwrap(),
            selection: Selection::all(),
        })
        .await;
    let courier = world.courier();
    assert_eq!(courier.run_once().await.unwrap(), 1);
    let emails = world.emails().await;
    let (confirm, _) = &emails[0];
    // The domain is not verified: the default sender, named after the page,
    // with the page's address to reply to.
    assert_eq!(confirm["from"], "\"Acme Status\" <alerts@parap.ly>");
    assert_eq!(confirm["reply_to"], json!(["status@mail.acme.com"]));
    assert_eq!(confirm["to"], json!(["reader@example.com"]));
    assert_eq!(
        confirm["subject"],
        "Confirm your subscription to Acme Status"
    );
    let link = link_in(confirm["text"].as_str().unwrap(), "/subscriptions/confirm");
    assert!(
        link.starts_with("https://console.example.com/s/acme/subscriptions/confirm?token="),
        "{link}"
    );
    let token = link.split("token=").nth(1).unwrap();
    let opened = world
        .store
        .open_subscriber_token(token, uptime_store::LinkPurpose::Confirm, world.clock.now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened.id, reader.id);
    world
        .store
        .confirm_subscriber(reader.id, world.clock.now())
        .await
        .unwrap();

    // Once the domain is verified, news goes from the page's own sender.
    world
        .store
        .set_sender_domain(
            world.page,
            Some(&SenderDomain {
                id: "dom_1".into(),
                name: "mail.acme.com".into(),
                status: DomainState::Verified,
                records: Vec::new(),
                checked_at: None,
            }),
        )
        .await
        .unwrap();
    world.clock.set(base() + SignedDuration::from_mins(5));
    world.declare("API errors", "api").await;
    assert_eq!(world.courier().run_once().await.unwrap(), 1);
    let emails = world.emails().await;
    let (news, request) = &emails[1];
    assert_eq!(news["from"], "\"Acme\" <status@mail.acme.com>");
    assert_eq!(news["subject"], "[Acme Status] Investigating: API errors");
    let html = news["html"].as_str().unwrap();
    assert!(html.contains("#ff3366"), "page accent");
    assert!(html.contains("Affected: Public API"), "page labels");
    assert!(html.contains("https://console.example.com/s/acme/incidents/"));
    let unsubscribe = news["headers"]["List-Unsubscribe"].as_str().unwrap();
    assert!(
        unsubscribe
            .starts_with("<https://console.example.com/s/acme/subscriptions/unsubscribe?token=")
    );
    assert_eq!(
        news["headers"]["List-Unsubscribe-Post"],
        "List-Unsubscribe=One-Click"
    );
    assert!(
        request
            .headers
            .get("idempotency-key")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("subscriber-message-")
    );
    // Sent once.
    assert_eq!(world.courier().run_once().await.unwrap(), 0);
}

#[tokio::test]
async fn links_use_the_pages_verified_custom_domain() {
    let world = World::new().await;
    let domain = world
        .store
        .add_domain(world.page, &"status.acme.com".parse().unwrap(), base())
        .await
        .unwrap();
    world
        .store
        .record_domain_check(domain.id, Ok(()), base())
        .await
        .unwrap();
    world
        .subscribe(NewSubscription::Email {
            email: "reader@example.com".parse().unwrap(),
            selection: Selection::all(),
        })
        .await;
    world.courier().run_once().await.unwrap();
    let (confirm, _) = &world.emails().await[0];
    let link = link_in(confirm["text"].as_str().unwrap(), "/subscriptions/confirm");
    assert!(
        link.starts_with("https://status.acme.com/subscriptions/confirm?token="),
        "{link}"
    );
}

#[tokio::test]
async fn webhooks_are_signed_and_disabled_after_failing_for_a_day() {
    let world = World::new().await;
    let url: url::Url = format!("{}/status-hook", world.hooks.uri())
        .parse()
        .unwrap();
    let hook = world
        .subscribe(NewSubscription::Webhook {
            url: url.clone(),
            email: "owner@example.com".parse().unwrap(),
            selection: Selection(vec!["db".into()]),
        })
        .await;
    world.courier().run_once().await.unwrap();
    let (confirm, _) = &world.emails().await[0];
    assert_eq!(confirm["to"], json!(["owner@example.com"]));
    assert!(
        confirm["text"]
            .as_str()
            .unwrap()
            .contains("to a webhook at 127.0.0.1")
    );
    world
        .store
        .confirm_subscriber(hook.id, world.clock.now())
        .await
        .unwrap();
    let secret = world
        .store
        .reveal_webhook_secret(hook.id)
        .await
        .unwrap()
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/status-hook"))
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(1)
        .mount(&world.hooks)
        .await;
    world.declare("API errors", "api").await; // not followed
    world.declare("Slow queries", "db").await;
    assert_eq!(world.courier().run_once().await.unwrap(), 1);
    let received = world.hooks.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let request = &received[0];
    let payload: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(payload["type"], "incident.created");
    assert_eq!(payload["page"]["slug"], "acme");
    assert_eq!(payload["data"]["title"], "Slow queries");
    assert_eq!(payload["data"]["components"], json!(["Database"]));
    let timestamp: i64 = request.headers["x-uptimestatus-timestamp"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        request.headers["x-uptimestatus-signature"]
            .to_str()
            .unwrap(),
        signature(&secret, timestamp, &request.body)
    );
    assert_eq!(request.headers["x-uptimestatus-event"], "incident.created");

    // From now on the endpoint fails: retried with backoff until it has
    // failed ten times over a day, then disabled and its owner told.
    Mock::given(method("POST"))
        .and(path("/status-hook"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&world.hooks)
        .await;
    let courier = world.courier();
    let mut at = world.clock.now();
    for _ in 0..40 {
        let subscriber = world.store.subscriber(hook.id).await.unwrap().unwrap();
        if subscriber.status == SubscriberStatus::Disabled {
            break;
        }
        world
            .store
            .declare_incident(
                &NewIncident {
                    title: format!("DB blip at {}", at.as_second()),
                    impact: Impact::Minor,
                    status: IncidentStatus::Investigating,
                    message: "Again.".into(),
                    monitors: vec!["db".parse().unwrap()],
                },
                at,
            )
            .await
            .unwrap();
        courier.run_once().await.unwrap();
        at += SignedDuration::from_hours(3);
        world.clock.set(at);
    }
    let disabled = world.store.subscriber(hook.id).await.unwrap().unwrap();
    assert_eq!(disabled.status, SubscriberStatus::Disabled);
    assert_eq!(
        disabled.last_error.as_deref(),
        Some("Webhook answered 500 Internal Server Error")
    );
    courier.run_once().await.unwrap();
    let emails = world.emails().await;
    let (notice, _) = emails.last().unwrap();
    assert_eq!(notice["to"], json!(["owner@example.com"]));
    assert_eq!(notice["subject"], "Acme Status: webhook disabled");
}

#[tokio::test]
async fn messages_for_ended_subscriptions_are_not_sent() {
    let world = World::new().await;
    let reader = world
        .subscribe(NewSubscription::Email {
            email: "reader@example.com".parse().unwrap(),
            selection: Selection::all(),
        })
        .await;
    world
        .store
        .confirm_subscriber(reader.id, world.clock.now())
        .await
        .unwrap();
    // The confirmation is still queued, but the subscriber is active now.
    world.courier().run_once().await.unwrap();
    assert!(world.emails().await.is_empty());
}
