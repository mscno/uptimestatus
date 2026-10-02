#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use uptime_domain::CheckSpec;
use uptime_domain::ConfigFile;
use uptime_server::seed;
use uptime_testkit::TestDb;

const SEED: &str = r#"
[[monitor]]
key = "example"
name = "Example"
[monitor.check]
type = "http"
url = "https://example.com/"
accepted_status = "200-299,301"
[monitor.policy]
interval = "30s"
retries = 2
retry_interval = "10s"

[[monitor]]
key = "db"
name = "Database port"
check = { type = "tcp", host = "db.example.com", port = 5432 }
"#;

#[test]
fn parses_monitors_with_human_durations() {
    let file = ConfigFile::from_toml(SEED).unwrap();

    assert_eq!(file.monitors.len(), 2);
    let example = &file.monitors[0];
    assert_eq!(example.key.as_str(), "example");
    assert_eq!(example.policy.interval, Duration::from_secs(30));
    assert_eq!(example.policy.retries, 2);
    assert!(matches!(&example.check, CheckSpec::Http(http) if http.accepted_status.contains(301)));
    assert!(matches!(&file.monitors[1].check, CheckSpec::Tcp(tcp) if tcp.port == 5432));
}

#[test]
fn rejects_unknown_fields() {
    let error = ConfigFile::from_toml("[[monitor]]\nkey = \"x\"\nname = \"x\"\ncheck = { type = \"tcp\", host = \"h\", port = 1 }\ncolour = \"red\"").unwrap_err();
    assert!(error.to_string().contains("colour"), "{error}");
}

#[tokio::test]
async fn creates_missing_monitors_and_skips_existing_ones() {
    let db = TestDb::new().await;
    let file = ConfigFile::from_toml(SEED).unwrap();
    let now = Timestamp::now();

    let first = seed::apply(db.store(), &file, now).await.unwrap();
    let second = seed::apply(db.store(), &file, now).await.unwrap();

    assert_eq!(first.created, ["example", "db"]);
    assert!(first.skipped.is_empty());
    assert!(second.created.is_empty());
    assert_eq!(second.skipped, ["example", "db"]);
    assert_eq!(db.store().list_monitors().await.unwrap().len(), 2);
}

#[tokio::test]
async fn first_checks_are_due_within_a_minute() {
    let db = TestDb::new().await;
    let now = Timestamp::now();
    seed::apply(db.store(), &ConfigFile::from_toml(SEED).unwrap(), now)
        .await
        .unwrap();

    for monitor in db.store().list_monitors().await.unwrap() {
        let due = db
            .store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .next_run_at;
        let offset = due.duration_since(now);
        assert!(
            offset >= SignedDuration::ZERO && offset < SignedDuration::from_secs(60),
            "{}: {offset}",
            monitor.spec.key
        );
    }
}

#[tokio::test]
async fn invalid_policies_are_rejected_before_anything_is_written() {
    let db = TestDb::new().await;
    let file = ConfigFile::from_toml(
        r#"
        [[monitor]]
        key = "ok"
        name = "fine"
        check = { type = "tcp", host = "h", port = 1 }

        [[monitor]]
        key = "too-fast"
        name = "bad"
        check = { type = "tcp", host = "h", port = 1 }
        policy = { interval = "5s" }
        "#,
    )
    .unwrap();

    let error = seed::apply(db.store(), &file, Timestamp::now())
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("too-fast"), "{error:#}");
    assert!(
        db.store().list_monitors().await.unwrap().is_empty(),
        "nothing written"
    );
}

#[test]
fn loads_from_a_file_and_names_missing_files() {
    let dir = std::env::temp_dir().join(format!("uptimestatus-seed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("monitors.toml");
    std::fs::write(&path, SEED).unwrap();

    let loaded = seed::load(&path).unwrap();
    let missing = seed::load(&dir.join("missing.toml")).unwrap_err();

    assert_eq!(loaded.monitors.len(), 2);
    assert!(
        format!("{missing:#}").contains("missing.toml"),
        "{missing:#}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

const WITH_PAGE: &str = r#"
[[monitor]]
key = "api"
name = "API"
check = { type = "tcp", host = "api.example.com", port = 443 }

[[page]]
slug = "platform"
title = "Platform"

[[page.section]]
name = "Core"
components = [{ monitor = "api", label = "Public API" }]
"#;

#[tokio::test]
async fn seeds_pages_and_exports_everything_back() {
    let db = TestDb::new().await;
    let file = ConfigFile::from_toml(WITH_PAGE).unwrap();

    let report = seed::apply(db.store(), &file, Timestamp::now())
        .await
        .unwrap();
    let again = seed::apply(db.store(), &file, Timestamp::now())
        .await
        .unwrap();

    assert_eq!(report.pages_created, ["platform"]);
    assert_eq!(again.pages_skipped, ["platform"]);
    let exported = db.store().export_config().await.unwrap();
    assert_eq!(exported, file, "export writes what seed reads");
    let text = exported.to_toml().unwrap();
    assert_eq!(ConfigFile::from_toml(&text).unwrap(), file);
}

#[tokio::test]
async fn pages_must_reference_known_monitors() {
    let db = TestDb::new().await;
    let file = ConfigFile::from_toml(
        "[[page]]\nslug = \"x\"\ntitle = \"X\"\n[[page.section]]\nname = \"S\"\ncomponents = [{ monitor = \"ghost\" }]",
    )
    .unwrap();
    let error = seed::apply(db.store(), &file, Timestamp::now())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("ghost"), "{error:#}");
}

#[tokio::test]
async fn the_example_seed_file_applies_cleanly() {
    let db = TestDb::new().await;
    let file = ConfigFile::from_toml(include_str!("../../../seed/monitors.example.toml")).unwrap();
    let report = seed::apply(db.store(), &file, Timestamp::now())
        .await
        .unwrap();
    assert_eq!(report.created.len(), file.monitors.len());
    assert_eq!(report.pages_created, ["platform"]);
}
