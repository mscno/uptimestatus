//! TLS-verified PostgreSQL snapshot export and atomic reverse restore.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use futures_util::{StreamExt as _, pin_mut};
use sha2::{Digest as _, Sha256};
use tokio_postgres::{Client, types::ToSql};

use crate::{
    columns, quote,
    schema::{Kind, TABLES, Table},
    snapshot::{self, Reader, Record, Report, Summary, Writer},
};

pub async fn connect(url: &str) -> Result<Client> {
    let (config, tls) = uptime_store::bus::connection_config(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection configuration"))?;
    let (client, connection) = config
        .connect(tls)
        .await
        .context("connecting to PostgreSQL")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn column_types(client: &Client, table: &Table) -> Result<Vec<String>> {
    let rows = client.query("SELECT column_name, udt_name FROM information_schema.columns WHERE table_schema='public' AND table_name=$1", &[&table.name]).await?;
    ensure!(
        rows.len() == table.columns.len(),
        "column count differs for {}",
        table.name
    );
    table
        .columns
        .iter()
        .map(|(name, kind)| {
            let row = rows
                .iter()
                .find(|row| row.get::<_, String>(0) == *name)
                .with_context(|| format!("missing column {}.{name}", table.name))?;
            let ty: String = row.get(1);
            let allowed = match kind {
                Kind::Integer => matches!(ty.as_str(), "int8" | "int4" | "int2"),
                Kind::Boolean => ty == "bool",
                Kind::Timestamp => ty == "timestamptz",
                Kind::Date => ty == "date",
                Kind::Text => matches!(ty.as_str(), "text" | "varchar"),
                Kind::Json => matches!(ty.as_str(), "text" | "varchar" | "json" | "jsonb"),
            };
            ensure!(allowed, "unsupported type for {}.{name}", table.name);
            Ok(ty)
        })
        .collect()
}

async fn check_schema(client: &Client) -> Result<()> {
    let rows = client
        .query(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid=c.oid AND d.deptype='e' AND d.refclassid='pg_extension'::regclass)",
            &[],
        )
        .await?;
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.get(0))
        .filter(|name: &String| name != "__toasty_migrations")
        .collect();
    ensure!(
        names.len() == TABLES.len() && TABLES.iter().all(|t| names.iter().any(|n| n == t.name)),
        "PostgreSQL application table inventory differs"
    );
    for table in TABLES {
        column_types(client, table).await?;
    }
    Ok(())
}

fn select(table: &Table) -> String {
    let expressions: Vec<_> = table
        .columns
        .iter()
        .map(|(name, kind)| {
            let name = quote(name);
            if *kind == Kind::Timestamp {
                format!(
                    "to_char({name} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"000Z\"')"
                )
            } else {
                format!("{name}::text")
            }
        })
        .collect();
    format!(
        "SELECT {} FROM {} ORDER BY {}",
        expressions.join(", "),
        quote(table.name),
        table
            .keys
            .iter()
            .map(|key| format!("{}.{}", quote(table.name), quote(key)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

async fn table_snapshot(
    client: &Client,
    table: &Table,
    mut writer: Option<&mut Writer>,
) -> Result<Summary> {
    let stream = client
        .query_raw(&select(table), std::iter::empty::<&(dyn ToSql + Sync)>())
        .await?;
    pin_mut!(stream);
    let mut hash = Sha256::new();
    let mut count = 0;
    while let Some(row) = stream.next().await {
        let row = row?;
        let values = table
            .columns
            .iter()
            .enumerate()
            .map(|(index, (_, kind))| snapshot::normalize(*kind, row.try_get(index)?))
            .collect::<Result<Vec<_>>>()?;
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

pub async fn export(url: &str, output: &Path) -> Result<Report> {
    let client = connect(url).await?;
    client
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL TIME ZONE 'UTC'")
        .await?;
    check_schema(&client).await?;
    let mut writer = Writer::new(output, "postgres")?;
    for table in TABLES {
        writer.push(&Record::Table {
            name: table.name.to_owned(),
        })?;
        let summary = table_snapshot(&client, table, Some(&mut writer)).await?;
        writer.push(&Record::TableEnd { summary })?;
    }
    client.batch_execute("COMMIT").await?;
    writer.finish()?;
    snapshot::validate(output)
}

/// Replaces application rows only with explicit authorization; rollback is atomic on failure.
pub async fn restore(
    url: &str,
    input: &Path,
    replace: bool,
    truncate_submicroseconds: bool,
) -> Result<Report> {
    let mut report = snapshot::validate(input)?;
    let store = uptime_store::Store::connect(url, &uptime_store::ConnectOptions::default()).await?;
    store.migrate().await?;
    drop(store);
    let client = connect(url).await?;
    check_schema(&client).await?;
    client
        .batch_execute("BEGIN; SET LOCAL TIME ZONE 'UTC'")
        .await?;
    // All access during a reverse cutover must already be quiesced. Locks enforce it.
    client
        .batch_execute(&format!(
            "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
            TABLES
                .iter()
                .map(|t| quote(t.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .await?;
    if !replace {
        for table in TABLES {
            let count: i64 = client
                .query_one(&format!("SELECT COUNT(*) FROM {}", quote(table.name)), &[])
                .await?
                .get(0);
            ensure!(
                count == 0,
                "destination is nonempty; explicit --replace is required"
            );
        }
    } else {
        for table in TABLES.iter().rev() {
            client
                .execute(&format!("DELETE FROM {}", quote(table.name)), &[])
                .await?;
        }
    }
    let mut reader = Reader::new(input)?;
    let mut table_index = 0;
    let mut statement = None;
    let mut changed_timestamps = 0u64;
    let mut expected_hash = Sha256::new();
    let mut expected_rows = 0;
    let mut normalized_summaries = Vec::new();
    while let Some((record, _)) = reader.read_record()? {
        match record {
            Record::Table { .. } => {
                expected_hash = Sha256::new();
                expected_rows = 0;
                let table = &TABLES[table_index];
                let types = column_types(&client, table).await?;
                let params = types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| format!("CAST(${}::text AS {ty})", i + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                statement = Some(
                    client
                        .prepare(&format!(
                            "INSERT INTO {} ({}) VALUES ({params})",
                            quote(table.name),
                            columns(table)
                        ))
                        .await?,
                );
            }
            Record::Row { mut values } => {
                for ((_, kind), value) in TABLES[table_index].columns.iter().zip(&mut values) {
                    if *kind == Kind::Timestamp
                        && let Some(text) = value
                    {
                        let timestamp = text.parse::<jiff::Timestamp>()?;
                        let nanos = timestamp.as_nanosecond();
                        if nanos % 1000 != 0 {
                            ensure!(
                                truncate_submicroseconds,
                                "PostgreSQL cannot preserve submicrosecond timestamps; explicit --truncate-submicroseconds is required"
                            );
                            *text = format!(
                                "{:.9}",
                                jiff::Timestamp::from_nanosecond(nanos.div_euclid(1000) * 1000)?
                            );
                            changed_timestamps += 1;
                        }
                    }
                }
                let params: Vec<&(dyn ToSql + Sync)> =
                    values.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
                client
                    .execute(statement.as_ref().context("row before table")?, &params)
                    .await?;
                snapshot::row_hash(&mut expected_hash, &values)?;
                expected_rows += 1;
            }
            Record::TableEnd { .. } => {
                normalized_summaries.push(Summary {
                    table: TABLES[table_index].name.to_owned(),
                    rows: expected_rows,
                    sha256: hex::encode(expected_hash.clone().finalize()),
                });
                table_index += 1;
            }
            _ => {}
        }
    }
    // Re-check the source seal before committing (including replacement restores).
    ensure!(
        snapshot::validate(input)? == report,
        "snapshot changed during restore"
    );
    for (table, expected) in TABLES.iter().zip(&normalized_summaries) {
        let actual = table_snapshot(&client, table, None).await?;
        ensure!(
            actual.rows == expected.rows,
            "restored row count differs for {}",
            table.name
        );
        ensure!(
            actual == *expected,
            "restored checksum differs for {}",
            table.name
        );
        if table.keys == ["id"] {
            client.query_one(&format!("SELECT setval(pg_get_serial_sequence($1, 'id'), GREATEST(COALESCE((SELECT MAX(id) FROM {}), 1), 1), EXISTS(SELECT 1 FROM {}))", quote(table.name), quote(table.name)), &[&table.name]).await?;
        }
    }
    client.batch_execute("COMMIT").await?;
    report.truncated_timestamps = changed_timestamps;
    report.tables = normalized_summaries;
    Ok(report)
}
