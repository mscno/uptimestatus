#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Database probes: PostgreSQL against the local test server, Redis against a
//! minimal in-process RESP server, and real MySQL and Redis servers when
//! `UPTIMESTATUS_TEST_MYSQL_URL` / `UPTIMESTATUS_TEST_REDIS_URL` are set.

use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
};
use uptime_domain::{CheckSpec, DatabaseCheck, FailureKind, Observation};
use uptime_probe::{AddressPolicy, Prober, ProberConfig};
use uptime_testkit::TestDb;

async fn prober(policy: AddressPolicy) -> Prober {
    let prober = Prober::new(ProberConfig {
        policy,
        ..ProberConfig::default()
    })
    .unwrap();
    prober.warm_up().await;
    prober
}

fn database(url: &str, query: Option<&str>, expect: Option<&str>) -> CheckSpec {
    CheckSpec::Database(DatabaseCheck {
        query: query.map(str::to_owned),
        expect: expect.map(str::to_owned),
        ..DatabaseCheck::url(url)
    })
}

async fn probe(policy: AddressPolicy, check: &CheckSpec) -> Observation {
    prober(policy)
        .await
        .probe(check, Duration::from_secs(10))
        .await
}

fn answered(observation: Observation) -> (String, Option<bool>) {
    match observation {
        Observation::Responded {
            response_body,
            keyword_found,
            ..
        } => (response_body.unwrap_or_default(), keyword_found),
        other => panic!("{other:?}"),
    }
}

fn failed(observation: Observation) -> (FailureKind, String) {
    match observation {
        Observation::Failed { kind, message } => (kind, message),
        other => panic!("{other:?}"),
    }
}

/// `postgres://user:pass@host:port/db` with the password replaced.
fn with_password(url: &str, password: &str) -> String {
    let mut url: url::Url = url.parse().unwrap();
    url.set_password(Some(password)).unwrap();
    url.to_string()
}

// ── PostgreSQL ───────────────────────────────────────────────────────────

#[tokio::test]
async fn postgres_queries_report_their_first_value() {
    let db = TestDb::new().await;
    let url = format!("{}?sslmode=disable", db.url().split('?').next().unwrap());
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await),
        ("1".to_owned(), None)
    );
    let custom = database(&url, Some("SELECT 'healthy', 2"), Some("health"));
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &custom).await),
        ("healthy".to_owned(), Some(true))
    );
}

#[tokio::test]
async fn postgres_failures_explain_themselves_without_secrets() {
    let db = TestDb::new().await;
    let base = format!("{}?sslmode=disable", db.url().split('?').next().unwrap());
    // Local test servers often trust loopback, so a missing database stands
    // in for a refused login.
    let mut missing: url::Url = with_password(&base, "s3cr3t").parse().unwrap();
    missing.set_path("/no_such_database");
    let (kind, message) = failed(
        probe(
            AddressPolicy::ALLOW_ALL,
            &database(missing.as_str(), None, None),
        )
        .await,
    );
    assert_eq!(kind, FailureKind::Io);
    assert!(message.contains("3D000"), "{message}");
    assert!(!message.contains("s3cr3t"), "{message}");

    let (kind, message) = failed(
        probe(
            AddressPolicy::ALLOW_ALL,
            &database(&base, Some("SELECT * FROM missing_table"), None),
        )
        .await,
    );
    assert_eq!(kind, FailureKind::Io);
    assert!(message.contains("missing_table"), "{message}");

    let (kind, _) = failed(probe(AddressPolicy::PUBLIC_ONLY, &database(&base, None, None)).await);
    assert_eq!(kind, FailureKind::Blocked);
}

#[tokio::test]
async fn postgres_without_tls_fails_a_required_tls_check() {
    let db = TestDb::new().await;
    let url = format!(
        "{}?sslmode=verify-full&sslrootcert=system",
        db.url().split('?').next().unwrap()
    );
    let (kind, _) = failed(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await);
    assert_eq!(kind, FailureKind::Tls);
}

// ── Redis (in-process) ───────────────────────────────────────────────────

/// Speaks just enough RESP: AUTH (password `secret`), SELECT, CLIENT, PING,
/// and GET (always `ok`).
async fn redis_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(resp(stream));
        }
    });
    port
}

async fn resp(stream: TcpStream) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    loop {
        let mut line = String::new();
        if read.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let count: usize = line.trim().trim_start_matches('*').parse().unwrap();
        let mut args = Vec::new();
        for _ in 0..count {
            let mut header = String::new();
            read.read_line(&mut header).await.unwrap();
            let len: usize = header.trim().trim_start_matches('$').parse().unwrap();
            let mut arg = vec![0; len + 2];
            read.read_exact(&mut arg).await.unwrap();
            args.push(String::from_utf8_lossy(&arg[..len]).into_owned());
        }
        let reply = match args[0].to_ascii_uppercase().as_str() {
            "AUTH" if args.last().map(String::as_str) == Some("secret") => "+OK\r\n".to_owned(),
            "AUTH" => "-WRONGPASS invalid username-password pair\r\n".to_owned(),
            "SELECT" | "CLIENT" => "+OK\r\n".to_owned(),
            "PING" => "+PONG\r\n".to_owned(),
            "GET" => "$2\r\nok\r\n".to_owned(),
            _ => "-ERR unknown command\r\n".to_owned(),
        };
        write.write_all(reply.as_bytes()).await.unwrap();
    }
}

#[tokio::test]
async fn redis_pings_and_runs_commands() {
    let port = redis_server().await;
    let url = format!("redis://:secret@127.0.0.1:{port}/2");
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await),
        ("PONG".to_owned(), None)
    );
    assert_eq!(
        answered(
            probe(
                AddressPolicy::ALLOW_ALL,
                &database(&url, Some("GET \"health key\""), Some("ok"))
            )
            .await
        ),
        ("ok".to_owned(), Some(true))
    );
}

#[tokio::test]
async fn redis_auth_failures_do_not_echo_the_password() {
    let port = redis_server().await;
    let url = format!("redis://:wrong-s3cr3t@127.0.0.1:{port}");
    let (kind, message) =
        failed(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await);
    assert_eq!(kind, FailureKind::Io);
    assert!(
        message.to_lowercase().contains("authentication failed"),
        "{message}"
    );
    assert!(!message.contains("s3cr3t"), "{message}");

    let (kind, _) = failed(probe(AddressPolicy::PUBLIC_ONLY, &database(&url, None, None)).await);
    assert_eq!(kind, FailureKind::Blocked);
}

#[tokio::test]
async fn closed_database_ports_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    for url in [
        format!("postgres://u:p@127.0.0.1:{port}/db?sslmode=disable"),
        format!("mysql://u:p@127.0.0.1:{port}/db"),
        format!("redis://127.0.0.1:{port}"),
    ] {
        let (kind, message) =
            failed(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await);
        assert_eq!(kind, FailureKind::Refused, "{url}: {message}");
    }
}

// ── Real servers (optional) ──────────────────────────────────────────────

/// e.g. `mysql://root:secret@127.0.0.1:3306/mysql`
#[tokio::test]
async fn mysql_server_answers_when_configured() {
    let Ok(url) = std::env::var("UPTIMESTATUS_TEST_MYSQL_URL") else {
        eprintln!("skipped: set UPTIMESTATUS_TEST_MYSQL_URL to test against MySQL");
        return;
    };
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await),
        ("1".to_owned(), None)
    );
    assert_eq!(
        answered(
            probe(
                AddressPolicy::ALLOW_ALL,
                &database(&url, Some("SELECT 'healthy'"), Some("healthy"))
            )
            .await
        ),
        ("healthy".to_owned(), Some(true))
    );
    let wrong = with_password(&url, "not-the-password-s3cr3t");
    let (kind, message) =
        failed(probe(AddressPolicy::ALLOW_ALL, &database(&wrong, None, None)).await);
    assert_eq!(kind, FailureKind::Io);
    assert!(message.contains("1045"), "{message}");
    assert!(!message.contains("s3cr3t"), "{message}");

    // A stock server's certificate is self-signed: verified TLS fails, and
    // passes when certificate errors are ignored.
    let tls = format!("{url}?require_ssl=true");
    let (kind, message) =
        failed(probe(AddressPolicy::ALLOW_ALL, &database(&tls, None, None)).await);
    assert_eq!(kind, FailureKind::Tls, "{message}");
    let lenient = CheckSpec::Database(DatabaseCheck {
        ignore_tls_errors: true,
        ..DatabaseCheck::url(&tls)
    });
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &lenient).await),
        ("1".to_owned(), None)
    );
}

/// e.g. `redis://127.0.0.1:6379`
#[tokio::test]
async fn redis_server_answers_when_configured() {
    let Ok(url) = std::env::var("UPTIMESTATUS_TEST_REDIS_URL") else {
        eprintln!("skipped: set UPTIMESTATUS_TEST_REDIS_URL to test against Redis");
        return;
    };
    assert_eq!(
        answered(probe(AddressPolicy::ALLOW_ALL, &database(&url, None, None)).await),
        ("PONG".to_owned(), None)
    );
    assert_eq!(
        answered(
            probe(
                AddressPolicy::ALLOW_ALL,
                &database(&url, Some("ECHO \"all good\""), Some("good"))
            )
            .await
        ),
        ("all good".to_owned(), Some(true))
    );
}
