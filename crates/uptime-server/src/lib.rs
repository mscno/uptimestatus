//! The `uptimestatus` binary's library half: everything `main` wires together,
//! exposed so it can be tested end to end.

pub mod cli;
pub mod config;
pub mod seed;
pub mod serve;
pub mod telemetry;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{Command, SecretsAction},
    config::Config,
};

/// Runs one CLI command to completion. `serve` stops when `shutdown` is cancelled.
pub async fn run(
    command: Command,
    config: Config,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    if command
        == (Command::Secrets {
            action: SecretsAction::GenerateKey,
        })
    {
        return generate_key();
    }
    let store = serve::connect(&config).await?;
    match command {
        Command::Migrate => {
            store.migrate().await.context("applying migrations")?;
            serve::prepare_secrets(&store).await?;
        }
        Command::Secrets {
            action: SecretsAction::Rotate,
        } => {
            anyhow::ensure!(
                store.encrypts_secrets(),
                "set secrets.keys (the new key first) before rotating"
            );
            store.ensure_secrets_readable().await?;
            let report = store.reseal().await.context("re-encrypting secrets")?;
            let active = config
                .secrets
                .keyring()?
                .map(|k| k.active_id().to_owned())
                .unwrap_or_default();
            store
                .record_audit(
                    &uptime_store::NewAuditEvent {
                        actor: "system".into(),
                        action: "secrets.rotate".into(),
                        target: None,
                        summary: format!(
                            "Re-encrypted {} secrets with key {active}",
                            report.total()
                        ),
                        details: serde_json::json!({
                            "key": active,
                            "sealed": report.sealed,
                            "indexed": report.indexed,
                        }),
                    },
                    jiff::Timestamp::now(),
                )
                .await?;
            tracing::info!(
                sealed = ?report.sealed,
                indexed = report.indexed,
                "secrets rotated; keys after the first can be removed"
            );
        }
        Command::Secrets {
            action: SecretsAction::GenerateKey,
        } => unreachable!("handled before connecting"),
        Command::Seed { path } => {
            store.ensure_secrets_readable().await?;
            let file = seed::load(&path)?;
            let report = seed::apply(&store, &file, jiff::Timestamp::now()).await?;
            tracing::info!(
                created = ?report.created,
                skipped = ?report.skipped,
                pages_created = ?report.pages_created,
                pages_skipped = ?report.pages_skipped,
                templates_created = ?report.templates_created,
                templates_skipped = ?report.templates_skipped,
                "seeded"
            );
        }
        Command::Export { output } => {
            store.ensure_secrets_readable().await?;
            let text = store.export_config().await?.to_toml()?;
            match output {
                Some(path) => std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?,
                None => std::io::Write::write_all(&mut std::io::stdout(), text.as_bytes())?,
            }
        }
        Command::Backup => {
            serve::prepare_secrets(&store).await?;
            let settings = config.backup_settings()?.ok_or_else(|| {
                anyhow::anyhow!("backups are disabled; set backup.enabled = true")
            })?;
            let backup = uptime_backup::Backup::new(store.clone(), settings)?;
            let run = backup.run_once().await?;
            tracing::info!(
                object_key = %run.report.object_key,
                bytes = run.report.archive_bytes,
                "backup encrypted; uploading"
            );
            run.upload.await??;
            tracing::info!("backup uploaded");
        }
        Command::Serve { migrate, roles } => {
            if migrate {
                store.migrate().await.context("applying migrations")?;
            }
            serve::prepare_secrets(&store).await?;
            if roles.worker {
                serve::reseal_in_background(store.clone());
            }
            serve::run(&config, store, roles, shutdown).await?;
        }
    }
    Ok(())
}

/// Prints a new `secrets.keys` entry.
pub fn generate_key() -> anyhow::Result<()> {
    let key = uptime_store::secrets::Keyring::generate();
    std::io::Write::write_all(&mut std::io::stdout(), format!("{key}\n").as_bytes())?;
    Ok(())
}
