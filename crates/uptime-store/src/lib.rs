//! PostgreSQL, SQLite and Turso persistence for uptimestatus.
//!
//! [`Store`] is the only way the rest of the workspace touches the database.
//! It speaks domain types; Toasty models and SQL stay inside this crate.

mod auth;
pub mod bus;
mod config;
mod convert;
mod domains;
mod error;
mod incidents;
mod maintenance;
mod models;
mod monitors;
mod notifications;
mod pages;
mod queue;
mod results;
mod tokens;

use jiff::{Timestamp, civil::Date};
use std::time::Duration;
use toasty_core::driver::operation::TransactionMode;

pub use auth::{AdminUser, GithubIdentity, NewSession, SessionInfo};
pub use domains::{CustomDomain, DomainStatus};
pub use error::StoreError;
pub use incidents::{Incident, IncidentUpdate, NewIncident};
pub use maintenance::Maintenance;
pub use monitors::{Monitor, MonitorOverview, RuntimeSnapshot};
pub use notifications::{Channel, Delivery, DeliveryLog, DeliveryStatus};
pub use pages::{Page, PageImage, PageSummary};
pub use queue::Claim;
pub use results::{CertUpdate, CheckRecord, StoredCheck, WINDOW_LIMIT, WindowCheck};
pub use toasty::migration::{MigrationReport, MigrationSet};
pub use tokens::{ApiToken, NewApiToken};

/// Every migration in `toasty/`, embedded at compile time.
pub static MIGRATIONS: MigrationSet = toasty::embed_migrations!();
pub static SQLITE_MIGRATIONS: MigrationSet = toasty::embed_migrations!("sqlite");

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Postgres,
    Sqlite,
    Turso,
}

impl Backend {
    pub fn from_url(url: &str) -> Result<Self> {
        match url.split_once(':').map(|(scheme, _)| scheme) {
            Some("postgres" | "postgresql") => Ok(Self::Postgres),
            Some("sqlite") => Ok(Self::Sqlite),
            Some("turso" | "libsql" | "https" | "http") => Ok(Self::Turso),
            _ => Err(StoreError::Configuration(
                "DATABASE_URL must use a PostgreSQL, SQLite or Turso URL scheme".into(),
            )),
        }
    }
}

/// Connection pool tuning.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub max_connections: usize,
    pub acquire_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            max_connections: 8,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// Handle to the database. Cheap to clone; clones share one connection pool.
#[derive(Clone, Debug)]
pub struct Store {
    db: toasty::Db,
    backend: Backend,
}

impl Store {
    /// Connects to the driver selected by the URL.
    pub async fn connect(url: &str, options: &ConnectOptions) -> Result<Self> {
        Self::connect_backend(url, Backend::from_url(url)?, None, options).await
    }

    /// Connects to an explicitly selected driver. Turso Cloud credentials are
    /// passed to the driver separately so they do not appear in the URL.
    pub async fn connect_backend(
        url: &str,
        backend: Backend,
        auth_token: Option<&str>,
        options: &ConnectOptions,
    ) -> Result<Self> {
        if Backend::from_url(url)? != backend {
            return Err(StoreError::Configuration(format!(
                "DATABASE_URL scheme does not match the configured {backend:?} backend"
            )));
        }
        if auth_token.is_some() && backend != Backend::Turso {
            return Err(StoreError::Configuration(
                "database.auth_token is only valid for Turso".into(),
            ));
        }
        let mut builder = Self::builder();
        builder
            .max_pool_size(if backend == Backend::Postgres {
                options.max_connections
            } else {
                1
            })
            .pool_wait_timeout(Some(options.acquire_timeout));
        let db = match backend {
            Backend::Postgres | Backend::Sqlite => builder.connect(url).await?,
            Backend::Turso => {
                let mut driver = toasty_driver_turso::Turso::new(url.to_owned())?;
                if let Some(token) = auth_token {
                    driver = driver.with_auth_token(token);
                }
                builder.build(driver).await?
            }
        };
        Ok(Self { db, backend })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub(crate) fn raw_timestamp(&self, value: Timestamp) -> toasty::stmt::Value {
        if self.backend == Backend::Postgres {
            value.into()
        } else {
            format!("{value:.9}").into()
        }
    }

    pub(crate) fn raw_date(&self, value: Date) -> toasty::stmt::Value {
        if self.backend == Backend::Postgres {
            value.into()
        } else {
            value.to_string().into()
        }
    }

    /// A Toasty builder with this crate's models registered.
    pub fn builder() -> toasty::db::Builder {
        let mut builder = toasty::Db::builder();
        builder.models(toasty::models!(crate::*));
        builder
    }

    /// Applies pending migrations.
    #[tracing::instrument(skip_all)]
    pub async fn migrate(&self) -> Result<MigrationReport> {
        let migrations = if self.backend == Backend::Postgres {
            &MIGRATIONS
        } else {
            &SQLITE_MIGRATIONS
        };
        let report = migrations.apply(&self.db).await?;
        tracing::info!(
            applied = report.applied(),
            skipped = report.skipped(),
            "migrations applied"
        );
        Ok(report)
    }

    /// Cheap readiness probe: one round trip to the database.
    pub async fn ping(&self) -> Result<()> {
        toasty::sql::query("SELECT 1")
            .exec(&mut self.db.clone())
            .await?;
        Ok(())
    }

    pub(crate) fn db(&self) -> toasty::Db {
        self.db.clone()
    }

    /// Cloud Turso databases using the classic SQLite engine need a plain
    /// `BEGIN`; the driver otherwise defaults to `BEGIN CONCURRENT`, which
    /// only TursoDB accepts. Our single-instance Turso mode uses deferred
    /// transactions on both engines.
    pub(crate) async fn transaction<'a>(
        &self,
        db: &'a mut toasty::Db,
    ) -> Result<toasty::db::Transaction<'a>> {
        if self.backend == Backend::Turso {
            Ok(db
                .transaction_builder()
                .mode(TransactionMode::Deferred)
                .begin()
                .await?)
        } else {
            Ok(db.transaction().await?)
        }
    }
}
