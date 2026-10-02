use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use uptime_transfer::{postgres, snapshot, turso};

#[derive(Parser)]
#[command(about = "Offline full database transfer. Snapshot files contain sensitive data.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read a consistent, read-only PostgreSQL snapshot. Uses DATABASE_URL, never a CLI credential.
    ExportPostgres {
        #[arg(long)]
        output: PathBuf,
    },
    /// Offline only: export the local database after stopping its application writer.
    ExportTurso {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Validate the snapshot's schema, normalized values, row counts and seal.
    Validate {
        #[arg(long)]
        input: PathBuf,
    },
    /// Create and verify a fresh database; refuses an existing destination.
    ImportTurso {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        reset_claims: bool,
    },
    /// Restore to PostgreSQL, including rollback after local writes. Uses DATABASE_URL.
    ImportPostgres {
        #[arg(long)]
        input: PathBuf,
        /// Atomically replace existing application rows. App writers MUST be stopped.
        #[arg(long)]
        replace: bool,
        /// PostgreSQL only stores microseconds; explicitly accept truncating finer precision.
        #[arg(long)]
        truncate_submicroseconds: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let report = match Cli::parse().command {
        Command::ExportPostgres { output } => {
            postgres::export(
                &std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
                &output,
            )
            .await?
        }
        Command::ExportTurso { database, output } => turso::export(&database, &output).await?,
        Command::Validate { input } => snapshot::validate(&input)?,
        Command::ImportTurso {
            input,
            database,
            reset_claims,
        } => turso::restore(&input, &database, reset_claims).await?,
        Command::ImportPostgres {
            input,
            replace,
            truncate_submicroseconds,
        } => {
            postgres::restore(
                &std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
                &input,
                replace,
                truncate_submicroseconds,
            )
            .await?
        }
    };
    std::io::Write::write_all(
        &mut std::io::stdout(),
        serde_json::to_string_pretty(&report)?.as_bytes(),
    )?;
    Ok(())
}
