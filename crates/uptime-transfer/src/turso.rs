//! Run with the app stopped: the pinned local engine is not opened by two processes.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use ::turso::{Connection, Value};
use anyhow::{Context as _, Result, ensure};
use sha2::{Digest as _, Sha256};

use crate::{
    columns, order, quote,
    schema::{Kind, TABLES, Table},
    snapshot::{self, Reader, Record, Report, Summary, Writer},
};

async fn open(path: &Path) -> Result<(::turso::Database, Connection)> {
    ensure!(path.is_file(), "local database file does not exist");
    let db = ::turso::Builder::new_local(path.to_str().context("non-UTF8 database path")?)
        .build()
        .await?;
    let connection = db.connect()?;
    Ok((db, connection))
}

async fn check_schema(connection: &Connection) -> Result<()> {
    let mut rows = connection.query("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT IN ('sqlite_sequence','__toasty_migrations') AND name NOT GLOB '__turso_internal_*' ORDER BY name", ()).await?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().await? {
        names.push(row.get::<String>(0)?);
    }
    ensure!(
        names.len() == TABLES.len() && TABLES.iter().all(|t| names.iter().any(|n| n == t.name)),
        "local application table inventory differs: {names:?}"
    );
    for table in TABLES {
        let mut info = connection
            .query(format!("PRAGMA table_info({})", quote(table.name)), ())
            .await?;
        let mut cols = Vec::new();
        while let Some(row) = info.next().await? {
            cols.push(row.get::<String>(1)?);
        }
        ensure!(
            cols.len() == table.columns.len()
                && table
                    .columns
                    .iter()
                    .all(|(c, _)| cols.iter().any(|n| n == c)),
            "local columns differ for {}",
            table.name
        );
    }
    Ok(())
}

async fn table_snapshot(
    connection: &Connection,
    table: &Table,
    mut writer: Option<&mut Writer>,
) -> Result<Summary> {
    let mut rows = connection
        .query(
            format!(
                "SELECT {} FROM {} ORDER BY {}",
                columns(table),
                quote(table.name),
                order(table)
            ),
            (),
        )
        .await?;
    let mut count = 0;
    let mut hash = Sha256::new();
    while let Some(row) = rows.next().await? {
        let mut values = Vec::new();
        for (index, (_, kind)) in table.columns.iter().enumerate() {
            let value = match row.get_value(index)? {
                Value::Null => None,
                Value::Integer(value) => Some(value.to_string()),
                Value::Text(value) => Some(value),
                _ => anyhow::bail!("unexpected stored value type"),
            };
            values.push(snapshot::normalize(*kind, value)?);
        }
        snapshot::row_hash(&mut hash, &values)?;
        if let Some(writer) = writer.as_mut() {
            writer.push(&Record::Row { values })?;
        }
        count += 1;
    }
    Ok(Summary {
        table: table.name.to_owned(),
        rows: count,
        sha256: hex::encode(hash.finalize()),
    })
}

pub async fn export(path: &Path, output: &Path) -> Result<Report> {
    let (_db, connection) = open(path).await?;
    check_schema(&connection).await?;
    connection.execute("BEGIN", ()).await?;
    let mut writer = Writer::new(output, "turso")?;
    for table in TABLES {
        writer.push(&Record::Table {
            name: table.name.to_owned(),
        })?;
        let summary = table_snapshot(&connection, table, Some(&mut writer)).await?;
        writer.push(&Record::TableEnd { summary })?;
    }
    connection.execute("COMMIT", ()).await?;
    writer.finish()?;
    snapshot::validate(output)
}

pub fn marker_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".verified.json");
    name.into()
}

pub async fn restore(input: &Path, path: &Path, reset_claims: bool) -> Result<Report> {
    let report = snapshot::validate(input)?;
    // Never overwrite a file or a prior completion marker.
    ensure!(
        !path.exists() && !marker_path(path).exists(),
        "destination already exists"
    );
    snapshot::private_file(path)?;
    let url = format!("turso:{}", path.to_str().context("non-UTF8 database path")?);
    let store =
        uptime_store::Store::connect(&url, &uptime_store::ConnectOptions::default()).await?;
    store.migrate().await?;
    drop(store);
    let (_db, connection) = open(path).await?;
    check_schema(&connection).await?;
    connection.execute("BEGIN", ()).await?;
    let mut reader = Reader::new(input)?;
    let mut table_index = 0;
    let mut statement = None;
    while let Some((record, _)) = reader.read_record()? {
        match record {
            Record::Table { .. } => {
                let table = &TABLES[table_index];
                let params = (1..=table.columns.len())
                    .map(|n| format!("?{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                statement = Some(
                    connection
                        .prepare(format!(
                            "INSERT INTO {} ({}) VALUES ({params})",
                            quote(table.name),
                            columns(table)
                        ))
                        .await?,
                );
            }
            Record::Row { values } => {
                let values = TABLES[table_index]
                    .columns
                    .iter()
                    .zip(values)
                    .map(|((_, kind), value)| match value {
                        None => Ok(Value::Null),
                        Some(value) if matches!(kind, Kind::Integer | Kind::Boolean) => {
                            Ok(Value::Integer(value.parse()?))
                        }
                        Some(value) => Ok(Value::Text(value)),
                    })
                    .collect::<Result<Vec<_>>>()?;
                statement
                    .as_mut()
                    .context("row before table")?
                    .execute(::turso::params_from_iter(values))
                    .await?;
            }
            Record::TableEnd { .. } => table_index += 1,
            _ => {}
        }
    }
    ensure!(
        snapshot::validate(input)? == report,
        "snapshot changed during restore"
    );
    for (table, expected) in TABLES.iter().zip(&report.tables) {
        ensure!(
            table_snapshot(&connection, table, None).await? == *expected,
            "restored checksum differs for {}",
            table.name
        );
        if table.keys == ["id"] {
            let sql = format!("SELECT COALESCE(MAX(id), 0) FROM {}", quote(table.name));
            let maximum = connection
                .query(sql, ())
                .await?
                .next()
                .await?
                .context("no maximum")?
                .get::<i64>(0)?;
            if maximum > 0 {
                let mut reseed = connection
                    .query(
                        format!(
                            "SELECT setval('__turso_internal_autoincrement_{}', ?1)",
                            table.name
                        ),
                        [maximum],
                    )
                    .await?;
                while reseed.next().await?.is_some() {}
            }
            let seq = connection
                .query(
                    format!(
                        "SELECT COALESCE(MAX(value),0) FROM {}",
                        quote(&format!(
                            "__turso_internal_seq___turso_internal_autoincrement_{}",
                            table.name
                        ))
                    ),
                    (),
                )
                .await?
                .next()
                .await?;
            ensure!(
                seq.is_some_and(|row| row.get::<i64>(0).is_ok_and(|n| n >= maximum))
                    || maximum == 0,
                "auto-increment sequence is behind imported IDs"
            );
        }
    }
    if reset_claims {
        connection
            .execute(
                "UPDATE monitor_runtime SET claimed_by=NULL, scheduled_for=NULL",
                (),
            )
            .await?;
    }
    connection.execute("COMMIT", ()).await?;
    drop(statement);
    // Checkpoint through the same engine; never assume a main-file-only copy is safe.
    let mut checkpoint = connection
        .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await?;
    while checkpoint.next().await?.is_some() {}
    drop(checkpoint);
    drop(connection);
    drop(_db);
    // Reopen to prove persistence before allowing the app to start.
    let store =
        uptime_store::Store::connect(&url, &uptime_store::ConnectOptions::default()).await?;
    ensure!(
        store.migrate().await?.applied() == 0,
        "unexpected pending migrations"
    );
    store.ping().await?;
    drop(store);
    let mut marker = snapshot::private_file(&marker_path(path))?;
    serde_json::to_writer_pretty(&mut marker, &report)?;
    marker.write_all(b"\n")?;
    marker.sync_all()?;
    Ok(report)
}
