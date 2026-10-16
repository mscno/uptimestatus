//! Consistent local snapshots, for encrypted off-volume backups.

use std::path::Path;

use crate::{Backend, Result, Store, StoreError, convert};

/// Row counts recorded with a backup, to prove the archive holds configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BackupCounts {
    pub monitors: i64,
    pub status_pages: i64,
    pub check_results: i64,
}

impl Store {
    /// Writes a transactionally consistent copy of the local database to `path`.
    ///
    /// `VACUUM INTO` takes a read snapshot of the whole database (including any
    /// uncheckpointed WAL) and writes a fresh, self-contained file, so writers
    /// need not be stopped first. The destination file must not already exist.
    pub async fn snapshot_to(&self, path: &Path) -> Result<()> {
        if self.backend() == Backend::Postgres {
            return Err(StoreError::Configuration(
                "backups snapshot a local SQLite or Turso database only".into(),
            ));
        }
        let text = path
            .to_str()
            .ok_or_else(|| StoreError::Configuration("snapshot path is not valid UTF-8".into()))?;
        if text.contains('\0') {
            return Err(StoreError::Configuration(
                "snapshot path contains a NUL byte".into(),
            ));
        }
        let literal = format!("'{}'", text.replace('\'', "''"));
        toasty::sql::statement(format!("VACUUM INTO {literal}"))
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// Monitors, status pages and check results, for the backup report.
    pub async fn backup_counts(&self) -> Result<BackupCounts> {
        let rows = toasty::sql::query(
            r#"SELECT (SELECT count(*) FROM "monitors"),
                      (SELECT count(*) FROM "status_pages"),
                      (SELECT count(*) FROM "check_results")"#,
        )
        .exec(&mut self.db())
        .await?;
        let row = rows
            .first()
            .ok_or_else(|| StoreError::corrupt("backup counts", "query returned no row"))?;
        let row = convert::Row::new("backup counts", row)?;
        Ok(BackupCounts {
            monitors: row.i64(0)?,
            status_pages: row.i64(1)?,
            check_results: row.i64(2)?,
        })
    }
}
