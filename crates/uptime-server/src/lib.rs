//! The `uptimestatus` binary's library half: everything `main` wires together,
//! exposed so it can be tested end to end.

pub mod cli;
pub mod config;
pub mod seed;
pub mod serve;
pub mod telemetry;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

use crate::{cli::Command, config::Config};

/// Runs one CLI command to completion. `serve` stops when `shutdown` is cancelled.
pub async fn run(
    command: Command,
    config: Config,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let store = serve::connect(&config).await?;
    match command {
        Command::Migrate => {
            store.migrate().await.context("applying migrations")?;
        }
        Command::Seed { path } => {
            let file = seed::load(&path)?;
            let report = seed::apply(&store, &file, jiff::Timestamp::now()).await?;
            tracing::info!(
                created = ?report.created,
                skipped = ?report.skipped,
                pages_created = ?report.pages_created,
                pages_skipped = ?report.pages_skipped,
                "seeded"
            );
        }
        Command::Export { output } => {
            let text = store.export_config().await?.to_toml()?;
            match output {
                Some(path) => std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?,
                None => std::io::Write::write_all(&mut std::io::stdout(), text.as_bytes())?,
            }
        }
        Command::Serve { migrate, roles } => {
            if migrate {
                store.migrate().await.context("applying migrations")?;
            }
            serve::run(&config, store, roles, shutdown).await?;
        }
    }
    Ok(())
}
