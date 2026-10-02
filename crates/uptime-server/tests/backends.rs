#![allow(clippy::unwrap_used)]

use std::{net::IpAddr, time::Duration};

use tokio_util::sync::CancellationToken;
use uptime_server::{
    cli::{Command, Roles},
    config::{
        AuthConfig, ChecksConfig, Config, DatabaseConfig, DomainsConfig, HttpConfig, LogConfig,
        Secret, StorageConfig,
    },
};
use uptime_store::Backend;

fn config(url: &str, backend: Backend) -> Config {
    Config {
        database_url: Secret::new(url),
        app_url: "http://status.localhost".parse().unwrap(),
        edge_host: "edge.localhost".into(),
        http: HttpConfig {
            host: IpAddr::from([127, 0, 0, 1]),
            port: 0,
            internal_port: 0,
        },
        database: DatabaseConfig {
            backend,
            ..DatabaseConfig::default()
        },
        log: LogConfig::default(),
        checks: ChecksConfig {
            poll_interval: Duration::from_millis(50),
            ..ChecksConfig::default()
        },
        auth: AuthConfig::default(),
        domains: DomainsConfig {
            verify: false,
            ..DomainsConfig::default()
        },
        storage: StorageConfig::default(),
    }
}

#[tokio::test]
async fn required_local_database_refuses_missing_file_and_report() {
    let path = std::env::temp_dir().join(format!("uptime-required-{}.db", std::process::id()));
    let mut config = config(&format!("turso:{}", path.display()), Backend::Turso);
    config.database.require_existing = true;
    assert!(uptime_server::serve::connect(&config).await.is_err());
    assert!(
        !path.exists(),
        "a guarded deployment must not create an empty database"
    );
    std::fs::write(&path, vec![0; 4096]).unwrap();
    assert!(uptime_server::serve::connect(&config).await.is_err());
    std::fs::remove_file(&path).unwrap();
}

async fn serves(url: &str, backend: Backend) {
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(uptime_server::run(
        Command::Serve {
            migrate: true,
            roles: Roles::ALL,
        },
        config(url, backend),
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!server.is_finished(), "server should keep running");
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn sqlite_serves_from_the_general_binary() {
    serves("sqlite::memory:", Backend::Sqlite).await;
}

#[tokio::test]
async fn turso_serves_from_the_general_binary() {
    serves("turso::memory:", Backend::Turso).await;
}

#[tokio::test]
async fn local_backends_require_combined_roles() {
    for (url, backend) in [
        ("sqlite::memory:", Backend::Sqlite),
        ("turso::memory:", Backend::Turso),
    ] {
        let error = uptime_server::run(
            Command::Serve {
                migrate: true,
                roles: Roles {
                    web: true,
                    worker: false,
                },
            },
            config(url, backend),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("both web and worker roles"));
    }
}
