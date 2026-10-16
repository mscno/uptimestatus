#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The CLI commands against a real database.

use std::{net::IpAddr, time::Duration};

use tokio_util::sync::CancellationToken;
use uptime_server::{
    cli::{Command, Roles, SecretsAction},
    config::{
        AuthConfig, ChecksConfig, Config, DatabaseConfig, DomainsConfig, HttpConfig, LogConfig,
        Secret, SecretsConfig, StorageConfig,
    },
};
use uptime_testkit::TestDb;

fn config(db: &TestDb) -> Config {
    Config {
        database_url: Secret::new(db.url()),
        app_url: "http://status.localhost".parse().unwrap(),
        edge_host: "edge.localhost".into(),
        http: HttpConfig {
            host: IpAddr::from([127, 0, 0, 1]),
            port: 0,
            internal_port: 0,
        },
        database: DatabaseConfig::default(),
        log: LogConfig::default(),
        checks: ChecksConfig {
            allow_private_targets: true,
            poll_interval: Duration::from_millis(50),
            ..ChecksConfig::default()
        },
        auth: AuthConfig::default(),
        domains: DomainsConfig {
            prewarm_tls: false,
            ..DomainsConfig::default()
        },
        storage: StorageConfig::default(),
        secrets: SecretsConfig {
            keys: Some(Secret::new(uptime_testkit::TEST_KEYS)),
        },
        email: Default::default(),
        backup: Default::default(),
    }
}

#[tokio::test]
async fn migrate_command_succeeds_on_a_migrated_database() {
    let db = TestDb::new().await;
    uptime_server::run(Command::Migrate, config(&db), CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn serve_command_runs_until_cancelled() {
    let db = TestDb::new().await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(uptime_server::run(
        Command::Serve {
            migrate: true,
            roles: Roles::ALL,
        },
        config(&db),
        shutdown.clone(),
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!server.is_finished(), "serve keeps running until shutdown");
    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("stops promptly")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn commands_refuse_sealed_secrets_without_keys() {
    let db = TestDb::new().await;
    db.store()
        .create_monitor(
            &uptime_domain::MonitorSpec {
                key: "db".parse().unwrap(),
                name: "Database".into(),
                check: uptime_domain::CheckSpec::Tcp(uptime_domain::TcpCheck::connect(
                    "db.example.com",
                    5432,
                )),
                policy: uptime_domain::CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    let mut keyless = config(&db);
    keyless.secrets = SecretsConfig::default();
    let error = uptime_server::run(Command::Migrate, keyless, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("secrets.keys"), "{error:#}");
}

#[tokio::test]
async fn secrets_rotate_re_encrypts_with_the_first_key() {
    let db = TestDb::new().await;
    let store = db.store();
    store
        .create_channel(
            &uptime_domain::ChannelSpec {
                name: "Ops".into(),
                kind: uptime_domain::ChannelKind::Webhook,
                url: "https://hooks.example.com/private".parse().unwrap(),
                secret: Some("signing".into()),
                default_on: false,
                routing: Default::default(),
            },
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    let new_key = uptime_store::secrets::Keyring::generate();
    let mut rotating = config(&db);
    rotating.secrets.keys = Some(Secret::new(format!(
        "{new_key},{}",
        uptime_testkit::TEST_KEYS
    )));
    let rotate = || Command::Secrets {
        action: SecretsAction::Rotate,
    };
    uptime_server::run(rotate(), rotating, CancellationToken::new())
        .await
        .unwrap();

    // Only the new key is needed now.
    let rotated = uptime_store::Store::connect(db.url(), &Default::default())
        .await
        .unwrap()
        .with_keyring(uptime_store::secrets::Keyring::parse(&new_key).unwrap());
    let channels = rotated.list_channels().await.unwrap();
    assert_eq!(channels[0].spec.secret.as_deref(), Some("signing"));
    let recorded = rotated
        .audit_events(&uptime_store::AuditQuery {
            action: Some("secrets.rotate".into()),
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].actor, "system");
    assert!(
        !recorded[0]
            .details
            .to_string()
            .contains(&new_key[new_key.len() - 10..])
    );

    // Rotating needs keys; generating one needs no database.
    let mut keyless = config(&db);
    keyless.secrets = SecretsConfig::default();
    let error = uptime_server::run(rotate(), keyless, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("secrets.keys"), "{error}");
    let mut offline = config(&db);
    offline.database_url = Secret::new("postgres://postgres:postgres@127.0.0.1:1/nope");
    uptime_server::run(
        Command::Secrets {
            action: SecretsAction::GenerateKey,
        },
        offline,
        CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn commands_fail_clearly_when_the_database_is_unreachable() {
    let db = TestDb::new().await;
    let mut config = config(&db);
    config.database_url = Secret::new("postgres://postgres:postgres@127.0.0.1:1/nope");
    let error = uptime_server::run(Command::Migrate, config, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("connecting to the database"),
        "{error:#}"
    );
}

#[tokio::test]
async fn serve_command_runs_the_scheduler() {
    use uptime_domain::{CheckPolicy, CheckSpec, HttpCheck, MonitorSpec};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

    let db = TestDb::new().await;
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&target)
        .await;
    let spec = MonitorSpec {
        key: "local-target".parse().unwrap(),
        name: "Local target".into(),
        check: CheckSpec::Http(HttpCheck::get(target.uri().parse().unwrap())),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    };
    let monitor = db
        .store()
        .create_monitor(&spec, jiff::Timestamp::now())
        .await
        .unwrap();
    let (logs, _capture) = uptime_testkit::logs::Logs::capture("debug");
    let shutdown = CancellationToken::new();
    let mut server = tokio::spawn(uptime_server::run(
        Command::Serve {
            migrate: false,
            roles: Roles::ALL,
        },
        config(&db),
        shutdown.clone(),
    ));

    let checked = async {
        while db
            .store()
            .recent_checks(monitor.id, 1)
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    // Say why when it fails: a server that already stopped, or what it logged.
    tokio::select! {
        outcome = &mut server => panic!("the server stopped early: {outcome:?}\n{:#?}", logs.lines()),
        result = tokio::time::timeout(Duration::from_secs(45), checked) => {
            assert!(result.is_ok(), "the scheduler did not check the monitor in time:\n{:#?}", logs.lines());
        }
    }

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("stops promptly")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn seed_command_creates_monitors_from_a_file() {
    let db = TestDb::new().await;
    let path =
        std::env::temp_dir().join(format!("uptimestatus-seed-cmd-{}.toml", std::process::id()));
    std::fs::write(&path, "[[monitor]]\nkey = \"db\"\nname = \"DB\"\ncheck = { type = \"tcp\", host = \"db.example.com\", port = 5432 }\n").unwrap();

    uptime_server::run(
        Command::Seed { path: path.clone() },
        config(&db),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let monitors = db.store().list_monitors().await.unwrap();
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0].spec.key.as_str(), "db");
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn seed_rejects_unworkable_checks_before_creating_anything() {
    let db = TestDb::new().await;
    let path = std::env::temp_dir().join(format!(
        "uptimestatus-seed-invalid-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        "[[monitor]]\nkey = \"edge\"\nname = \"Edge\"\ncheck = { type = \"ping\", host = \"edge.example.com\" }\n\n\
         [[monitor]]\nkey = \"docs\"\nname = \"Docs\"\ncheck = { type = \"database\", url = \"mongodb://docs.example.com\" }\n",
    )
    .unwrap();

    let error = uptime_server::run(
        Command::Seed { path: path.clone() },
        config(&db),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(format!("{error:#}").contains("monitor `docs`"), "{error:#}");
    assert!(db.store().list_monitors().await.unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn roles_split_checking_from_serving() {
    use uptime_domain::{CheckPolicy, CheckSpec, MonitorSpec, TcpCheck};

    let db = TestDb::new().await;
    let monitor = db
        .store()
        .create_monitor(
            &MonitorSpec {
                key: "db".parse().unwrap(),
                name: "Database".into(),
                check: CheckSpec::Tcp(TcpCheck::connect("127.0.0.1", 9)),
                policy: CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    let checked = || async {
        !db.store()
            .recent_checks(monitor.id, 1)
            .await
            .unwrap()
            .is_empty()
    };
    let shutdown = CancellationToken::new();
    let web = Roles {
        web: true,
        worker: false,
    };
    let web_instance = tokio::spawn(uptime_server::run(
        Command::Serve {
            migrate: false,
            roles: web,
        },
        config(&db),
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!checked().await, "a web-only instance runs no checks");

    let worker = Roles {
        web: false,
        worker: true,
    };
    let worker_instance = tokio::spawn(uptime_server::run(
        Command::Serve {
            migrate: false,
            roles: worker,
        },
        config(&db),
        shutdown.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(45), async {
        while !checked().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the worker checks the monitor");

    shutdown.cancel();
    for instance in [web_instance, worker_instance] {
        tokio::time::timeout(Duration::from_secs(5), instance)
            .await
            .expect("stops promptly")
            .unwrap()
            .unwrap();
    }
}
