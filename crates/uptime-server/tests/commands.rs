#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The CLI commands against a real database.

use std::{net::IpAddr, time::Duration};

use tokio_util::sync::CancellationToken;
use uptime_server::{
    cli::{Command, Roles},
    config::{
        AuthConfig, ChecksConfig, Config, DatabaseConfig, DomainsConfig, HttpConfig, LogConfig,
        Secret, StorageConfig,
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
