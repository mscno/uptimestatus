#![allow(clippy::unwrap_used)]

use std::{fs, path::PathBuf};
use tokio_postgres::types::ToSql;
use uptime_testkit::TestDb;
use uptime_transfer::{
    postgres,
    schema::{Kind, TABLES},
    snapshot, turso,
};

fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!("uptime-transfer-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&path).unwrap();
    path
}

// Cover every column of every table, including secrets, Unicode, large IDs and JSON.
async fn fixture(db: &TestDb) {
    let client = postgres::connect(db.url()).await.unwrap();
    for table in TABLES {
        let column_info = client.query("SELECT column_name, udt_name, is_nullable FROM information_schema.columns WHERE table_schema='public' AND table_name=$1", &[&table.name]).await.unwrap();
        let types: Vec<String> = table
            .columns
            .iter()
            .map(|(name, _)| {
                column_info
                    .iter()
                    .find(|r| r.get::<_, String>(0) == *name)
                    .unwrap()
                    .get(1)
            })
            .collect();
        let values: Vec<Option<String>> = table
            .columns
            .iter()
            .map(|(name, kind)| {
                if matches!(
                    *name,
                    "last_error" | "favicon" | "repeat_until" | "last_push_message"
                ) {
                    return None;
                }
                Some(match kind {
                    Kind::Integer => if *name == "id" || name.ends_with("_id") {
                        "9223372036854700"
                    } else {
                        "42"
                    }
                    .to_owned(),
                    Kind::Boolean => if *name == "invert" { "false" } else { "true" }.to_owned(),
                    Kind::Timestamp => "2026-10-02T12:34:56.123456000Z".to_owned(),
                    Kind::Date => "2026-10-02".to_owned(),
                    Kind::Json => r#"{"z":[true,null,9223372036854700],"a":"å\"\\\n"}"#.to_owned(),
                    Kind::Text => format!("{name}: Øresund \"quoted\" \\ line\nsecond"),
                })
            })
            .collect();
        let cols = table
            .columns
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(",");
        let args = types
            .iter()
            .enumerate()
            .map(|(i, ty)| format!("CAST(${}::text AS {ty})", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let params: Vec<&(dyn ToSql + Sync)> =
            values.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
        client
            .execute(
                &format!("INSERT INTO \"{}\" ({cols}) VALUES ({args})", table.name),
                &params,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn all_tables_roundtrip_and_new_writes_survive_reverse_restore() {
    let source = TestDb::new().await;
    fixture(&source).await;
    // A projected `id::text` alias must never change numeric source ordering.
    let client = postgres::connect(source.url()).await.unwrap();
    for id in [2_i64, 10_i64] {
        client.execute("INSERT INTO admin_users (id, github_id, login, created_at, last_login_at) VALUES ($1,$1,$2,NOW(),NOW())", &[&id, &id.to_string()]).await.unwrap();
    }
    drop(client);
    let dir = directory();
    let exported = dir.join("postgres.jsonl");
    let local = dir.join("database.db");
    let source_report = postgres::export(source.url(), &exported).await.unwrap();
    assert!(
        source_report
            .tables
            .iter()
            .all(|table| table.rows == if table.table == "admin_users" { 3 } else { 1 })
    );
    let imported = turso::restore(&exported, &local, false).await.unwrap();
    assert_eq!(source_report, imported);
    assert!(turso::marker_path(&local).is_file());
    assert!(turso::restore(&exported, &local, false).await.is_err());
    let local_snapshot = dir.join("local.jsonl");
    assert_eq!(
        turso::export(&local, &local_snapshot).await.unwrap().tables,
        source_report.tables
    );
    let rollback = TestDb::new().await;
    assert_eq!(
        postgres::restore(rollback.url(), &local_snapshot, false, false)
            .await
            .unwrap()
            .tables,
        source_report.tables
    );
    assert!(
        postgres::restore(rollback.url(), &local_snapshot, false, false)
            .await
            .is_err()
    );

    let db = ::turso::Builder::new_local(local.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    let id: i64 = conn.query("INSERT INTO admin_users (github_id, login, created_at, last_login_at) VALUES(777,'new','2026-10-03T00:00:00.000000000Z','2026-10-03T00:00:00.000000000Z') RETURNING id", ()).await.unwrap().next().await.unwrap().unwrap().get(0).unwrap();
    assert!(id > 9223372036854700);
    conn.execute(
        "UPDATE monitors SET updated_at='2026-10-03T00:00:00.123456789Z'",
        (),
    )
    .await
    .unwrap();
    drop(conn);
    drop(db);
    let newer = dir.join("newer.jsonl");
    turso::export(&local, &newer).await.unwrap();
    assert!(
        postgres::restore(rollback.url(), &newer, true, false)
            .await
            .is_err()
    );
    // A failed reverse transfer leaves the original destination intact.
    let intact = dir.join("intact.jsonl");
    assert_eq!(
        postgres::export(rollback.url(), &intact)
            .await
            .unwrap()
            .tables,
        source_report.tables
    );
    let restored = postgres::restore(rollback.url(), &newer, true, true)
        .await
        .unwrap();
    assert_eq!(restored.truncated_timestamps, 1);
    let checked = dir.join("checked.jsonl");
    assert_eq!(
        postgres::export(rollback.url(), &checked)
            .await
            .unwrap()
            .tables,
        restored.tables
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&exported).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn corrupt_and_incomplete_snapshots_cannot_create_a_database() {
    let source = TestDb::new().await;
    let dir = directory();
    let exported = dir.join("snapshot.jsonl");
    postgres::export(source.url(), &exported).await.unwrap();
    let bytes = fs::read(&exported).unwrap();
    fs::write(&exported, &bytes[..bytes.len() - 100]).unwrap();
    let local = dir.join("database.db");
    assert!(snapshot::validate(&exported).is_err());
    assert!(turso::restore(&exported, &local, false).await.is_err());
    assert!(!local.exists());
    let mut bytes = bytes;
    let index = bytes.iter().position(|b| *b == b'1').unwrap();
    bytes[index] = b'9';
    fs::write(&exported, bytes).unwrap();
    assert!(snapshot::validate(&exported).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn unknown_source_columns_are_rejected() {
    let source = TestDb::new().await;
    let client = postgres::connect(source.url()).await.unwrap();
    client
        .batch_execute("ALTER TABLE monitors ADD COLUMN surprise TEXT")
        .await
        .unwrap();
    let dir = directory();
    assert!(
        postgres::export(source.url(), &dir.join("snapshot.jsonl"))
            .await
            .is_err()
    );
    fs::remove_dir_all(dir).unwrap();
}
