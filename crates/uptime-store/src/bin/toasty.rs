//! Migration CLI for the store's Toasty models.
//!
//! `mise run db:migration:new <name>` diffs the PostgreSQL models against the
//! latest snapshot. Set `UPTIMESTATUS_DATABASE__BACKEND=sqlite` to generate
//! the SQLite/Turso counterpart in `crates/uptime-store/sqlite/`.
//! Other subcommands: `migration apply | snapshot | drop | reset`.

use anyhow::Context as _;
use toasty_cli::{Config, MigrationConfig, ToastyCli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend =
        std::env::var("UPTIMESTATUS_DATABASE__BACKEND").unwrap_or_else(|_| "postgres".to_owned());
    let (url, directory) = match backend.as_str() {
        "postgres" => (
            std::env::var("DATABASE_URL").context("DATABASE_URL must point at the dev database")?,
            "toasty",
        ),
        "sqlite" | "turso" => ("sqlite::memory:".to_owned(), "sqlite"),
        _ => anyhow::bail!("database backend must be postgres, sqlite or turso"),
    };
    let db = uptime_store::Store::builder().connect(&url).await?;
    let config = Config::new().migration(
        MigrationConfig::new()
            .path(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(directory)),
    );
    ToastyCli::with_config(db, config).parse_and_run().await
}
