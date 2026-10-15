#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Secrets are encrypted at rest, readable through the store, and sealed or
//! re-encrypted when the keyring changes.

mod common;

use std::time::Duration;

use common::{http_spec, t};
use pretty_assertions::assert_eq;
use uptime_domain::{
    ChannelKind, ChannelSpec, CheckSpec, Health, HttpAuth, MonitorId, MonitorSpec, MonitorState,
    Push, PushCheck, Runtime, Transition, Verdict,
};
use uptime_store::{CheckRecord, ConnectOptions, Store, secrets::Keyring};
use uptime_testkit::TestDb;

const PASSWORD: &str = "hunter2-very-secret";
const HEADER: &str = "Bearer header-secret";
const TOKEN: &str = "push-token-secret";
const HOOK: &str = "https://hooks.slack.com/services/T0/B0/hook-secret";
const SIGNING: &str = "signing-secret";
const BODY: &str = "upstream body with a private detail";

fn key(id: &str, byte: u8) -> String {
    use base64::Engine as _;
    format!(
        "{id}:{}",
        base64::engine::general_purpose::STANDARD.encode([byte; 32])
    )
}

async fn unkeyed(db: &TestDb) -> Store {
    Store::connect(db.url(), &ConnectOptions::default())
        .await
        .unwrap()
}

async fn keyed(db: &TestDb, keys: &str) -> Store {
    unkeyed(db)
        .await
        .with_keyring(Keyring::parse(keys).unwrap())
}

/// Writes one of everything that holds a secret.
async fn populate(store: &Store) -> MonitorId {
    let mut api = http_spec("api");
    if let CheckSpec::Http(http) = &mut api.check {
        http.auth = Some(HttpAuth::Basic {
            username: "ops".into(),
            password: PASSWORD.into(),
        });
        http.headers = vec![("Authorization".into(), HEADER.into())];
    }
    let api = store.create_monitor(&api, t(0)).await.unwrap();
    store
        .create_monitor(
            &MonitorSpec {
                check: CheckSpec::Push(PushCheck {
                    token: TOKEN.into(),
                }),
                ..http_spec("cron")
            },
            t(0),
        )
        .await
        .unwrap();
    store
        .create_channel(
            &ChannelSpec {
                name: "Ops".into(),
                kind: ChannelKind::Webhook,
                url: HOOK.parse().unwrap(),
                secret: Some(SIGNING.into()),
                default_on: false,
                routing: Default::default(),
            },
            t(0),
        )
        .await
        .unwrap();
    for (at, state, failures) in [(10, MonitorState::Pending, 1), (20, MonitorState::Down, 2)] {
        store
            .record_check(&CheckRecord {
                monitor_id: api.id,
                scheduled_for: t(at),
                checked_at: t(at),
                verdict: Verdict {
                    health: Health::Down,
                    latency: Some(Duration::from_millis(9)),
                    status_code: Some(503),
                    reason: Some(uptime_domain::DownReason::UnexpectedStatus(503)),
                    response_body: Some(BODY.into()),
                },
                runtime: Runtime {
                    state,
                    consecutive_failures: failures,
                },
                transition: (failures == 2).then_some(Transition::WentDown),
                cert: None,
                next_run_at: t(at + 20),
                region: "fra".into(),
            })
            .await
            .unwrap();
    }
    api.id
}

/// Every stored value of the sealed columns, straight from the database.
async fn raw_values(db: &TestDb) -> Vec<String> {
    let (client, connection) = tokio_postgres::connect(db.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut values = Vec::new();
    for sql in [
        r#"SELECT "check" FROM monitors"#,
        "SELECT url FROM notification_channels",
        "SELECT secret FROM notification_channels WHERE secret IS NOT NULL",
        "SELECT response_body FROM check_results WHERE response_body IS NOT NULL",
        "SELECT check_json FROM incident_updates WHERE check_json IS NOT NULL",
    ] {
        for row in client.query(sql, &[]).await.unwrap() {
            values.push(row.get::<_, String>(0));
        }
    }
    values
}

fn assert_no_secrets(values: &[String]) {
    assert!(values.len() >= 8, "{values:?}");
    for value in values {
        assert!(value.starts_with("enc:v1:"), "not sealed: {value}");
        for secret in [PASSWORD, HEADER, TOKEN, "hook-secret", SIGNING, BODY] {
            assert!(!value.contains(secret), "{secret} leaked: {value}");
        }
    }
}

async fn assert_readable(store: &Store, api: MonitorId) {
    let monitor = store.monitor(api).await.unwrap().unwrap();
    let CheckSpec::Http(http) = &monitor.spec.check else {
        panic!("an HTTP check")
    };
    assert_eq!(
        http.auth,
        Some(HttpAuth::Basic {
            username: "ops".into(),
            password: PASSWORD.into()
        })
    );
    let channel = &store.list_channels().await.unwrap()[0];
    assert_eq!(channel.spec.url.as_str(), HOOK);
    assert_eq!(channel.spec.secret.as_deref(), Some(SIGNING));
    let recent = store.recent_checks(api, 1).await.unwrap();
    assert_eq!(recent[0].response_body.as_deref(), Some(BODY));
    let incident = store.list_incidents(1).await.unwrap().remove(0);
    let check = incident
        .updates
        .iter()
        .find_map(|u| u.check.as_ref())
        .unwrap();
    assert_eq!(check.response_body.as_deref(), Some(BODY));
    let push = Push {
        at: t(30),
        up: true,
        message: None,
        ping: None,
    };
    assert!(store.record_push(TOKEN, &push).await.unwrap().is_some());
    assert_eq!(store.record_push("wrong-token", &push).await.unwrap(), None);
}

#[tokio::test]
async fn secrets_are_sealed_at_rest_and_read_back() {
    let db = TestDb::new().await;
    let api = populate(db.store()).await;

    assert_no_secrets(&raw_values(&db).await);
    assert_readable(db.store(), api).await;
    assert_eq!(db.store().plaintext_secrets().await.unwrap(), 0);
    assert_eq!(
        db.store().reseal().await.unwrap().total(),
        0,
        "nothing to do"
    );

    // An instance without the keys refuses to start rather than fail later.
    let error = unkeyed(&db)
        .await
        .ensure_secrets_readable()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("secrets.keys"), "{error}");
    db.store().ensure_secrets_readable().await.unwrap();
}

#[tokio::test]
async fn plaintext_from_before_encryption_is_sealed() {
    let db = TestDb::new().await;
    let legacy = unkeyed(&db).await;
    let api = populate(&legacy).await;
    legacy.ensure_secrets_readable().await.unwrap();
    assert!(raw_values(&db).await.iter().all(|v| !v.starts_with("enc:")));

    // Keyed instances read plaintext as-is, then seal it.
    let store = db.store();
    assert!(store.plaintext_secrets().await.unwrap() >= 8);
    let report = store.reseal().await.unwrap();
    assert_eq!(report.indexed, 1, "the push token index now uses the key");
    assert_eq!(
        report
            .sealed
            .iter()
            .map(|(column, _)| column.as_str())
            .collect::<Vec<_>>(),
        [
            "monitors.check",
            "notification_channels.url",
            "notification_channels.secret",
            "check_results.response_body",
            "incident_updates.check_json"
        ]
    );
    assert_eq!(store.plaintext_secrets().await.unwrap(), 0);
    assert_no_secrets(&raw_values(&db).await);
    assert_readable(store, api).await;
    assert_eq!(store.reseal().await.unwrap().total(), 0, "idempotent");
}

#[tokio::test]
async fn rotation_re_encrypts_with_the_new_key() {
    let db = TestDb::new().await;
    let old = keyed(&db, &key("old", 1)).await;
    let api = populate(&old).await;
    assert!(
        raw_values(&db)
            .await
            .iter()
            .all(|v| v.starts_with("enc:v1:old:"))
    );

    let rotating = keyed(&db, &format!("{},{}", key("new", 2), key("old", 1))).await;
    assert_readable(&rotating, api).await;
    let report = rotating.reseal().await.unwrap();
    assert!(report.sealed.iter().all(|(_, n)| *n > 0), "{report:?}");
    assert!(
        raw_values(&db)
            .await
            .iter()
            .all(|v| v.starts_with("enc:v1:new:"))
    );

    // The old key can now be dropped.
    let rotated = keyed(&db, &key("new", 2)).await;
    assert_readable(&rotated, api).await;
    let error = old.monitor(api).await.unwrap_err();
    assert!(error.to_string().contains("not in secrets.keys"), "{error}");
}
