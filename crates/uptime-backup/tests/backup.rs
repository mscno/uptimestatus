//! End-to-end: a real local database is snapshotted, encrypted and uploaded.

use std::{path::PathBuf, time::Duration};

use uptime_backup::{Backup, BackupSettings, S3Settings, TimeOfDay};
use uptime_store::{ConnectOptions, Store};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

/// A valid age recipient; the matching identity stays off the machine, so the
/// test never needs to decrypt.
const RECIPIENT: &str = "age1znvnm9uvkkpc7ffymem9qhevh9n08nfjh2dpyxylxk53eay6xe9qvp6lfr";

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uptime-backup-it-{tag}-{}", std::process::id()))
}

#[tokio::test]
async fn a_run_snapshots_encrypts_and_uploads() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2) // the archive and its report
        .mount(&server)
        .await;

    let dir = temp_dir("run");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let database = dir.join("uptimestatus.db");
    let store = Store::connect(
        &format!("sqlite:{}", database.display()),
        &ConnectOptions::default(),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let counts = store.backup_counts().await.expect("counts");
    assert_eq!(counts.monitors, 0);

    let settings = BackupSettings {
        s3: S3Settings {
            endpoint: server.uri(),
            region: "auto".into(),
            bucket: "backups".into(),
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
        },
        recipient: RECIPIENT.into(),
        prefix: "uptimestatus".into(),
        database_path: database.clone(),
        uploads: None,
        write_timeout: Duration::from_secs(30),
        at: TimeOfDay::parse("03:17").unwrap(),
    };
    let backup = Backup::new(store.clone(), settings).expect("backup");
    let run = backup.run_once().await.expect("run");
    assert_eq!(run.report.format, 1);
    assert!(run.report.object_key.starts_with("uptimestatus/backup-"));
    assert!(run.report.object_key.ends_with(".volume.tar.gz.age"));
    assert_eq!(run.report.archive_sha256.len(), 64);
    assert!(run.report.archive_bytes > 0);
    run.upload.await.expect("join").expect("upload");

    // Writes are open again and no temporary files remain.
    assert!(!store.writes_paused());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("backup-"))
        .map(|entry| entry.file_name())
        .collect();
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_upload_failure_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = temp_dir("fail");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let store = Store::connect(
        &format!("sqlite:{}", dir.join("uptimestatus.db").display()),
        &ConnectOptions::default(),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");

    let settings = BackupSettings {
        s3: S3Settings {
            endpoint: server.uri(),
            region: "auto".into(),
            bucket: "backups".into(),
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
        },
        recipient: RECIPIENT.into(),
        prefix: String::new(),
        database_path: dir.join("uptimestatus.db"),
        uploads: None,
        write_timeout: Duration::from_secs(30),
        at: TimeOfDay::parse("03:17").unwrap(),
    };
    let backup = Backup::new(store, settings).expect("backup");
    let run = backup.run_once().await.expect("the local archive succeeds");
    assert!(
        run.upload.await.expect("join").is_err(),
        "a 500 must fail the upload"
    );

    std::fs::remove_dir_all(&dir).ok();
}
