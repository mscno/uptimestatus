//! Bringing stored secrets in line with the keyring: sealing legacy plaintext
//! and re-encrypting values sealed with an older key (rotation).

use crate::{
    Backend, Result, Store, StoreError, convert,
    convert::{Row, like_escape},
    models::MonitorRecord,
    secrets::{SEALED_COLUMNS, is_sealed},
};

/// Rows read per batch.
const BATCH: i64 = 200;

/// What [`Store::reseal`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResealReport {
    /// Values (re)sealed per `"table.column"`, for columns that changed.
    pub sealed: Vec<(String, u64)>,
    /// Push monitors and subscribers whose blind indexes were recomputed.
    pub indexed: u64,
}

impl ResealReport {
    pub fn total(&self) -> u64 {
        self.sealed.iter().map(|(_, n)| n).sum::<u64>() + self.indexed
    }
}

impl Store {
    /// Fails when the database holds sealed secrets but no keyring is
    /// configured, so a misconfigured instance stops at startup rather than
    /// on first use. Checks the small configuration tables.
    pub async fn ensure_secrets_readable(&self) -> Result<()> {
        if self.sealer().keyring().is_some() {
            return Ok(());
        }
        for (table, column) in [
            ("monitors", "check"),
            ("notification_channels", "url"),
            ("notification_channels", "secret"),
        ] {
            let sql = self.dialect(&format!(
                r#"SELECT 1 FROM "{table}" WHERE "{column}" LIKE 'enc:v1:%' LIMIT 1"#
            ));
            if !toasty::sql::query(sql)
                .exec(&mut self.db())
                .await?
                .is_empty()
            {
                return Err(StoreError::Configuration(format!(
                    "{table}.{column} holds encrypted secrets, but secrets.keys is not set"
                )));
            }
        }
        Ok(())
    }

    /// Seals every plaintext value in a sealed column and re-encrypts values
    /// sealed with an older key, in batches; recomputes push token indexes.
    /// Idempotent and safe to run while serving (rows changed concurrently
    /// are left alone). Does nothing without a keyring.
    #[tracing::instrument(skip(self))]
    pub async fn reseal(&self) -> Result<ResealReport> {
        self.reseal_columns(SEALED_COLUMNS).await
    }

    /// [`Store::reseal`] for the small configuration tables only (monitors
    /// and channels) and push token indexes: quick enough to finish before
    /// serving, so pushes and alerts work at once after a key change.
    #[tracing::instrument(skip(self))]
    pub async fn reseal_config(&self) -> Result<ResealReport> {
        let config: Vec<_> = SEALED_COLUMNS
            .iter()
            .copied()
            .filter(|(table, _)| matches!(*table, "monitors" | "notification_channels"))
            .collect();
        self.reseal_columns(&config).await
    }

    async fn reseal_columns(&self, columns: &[(&str, &str)]) -> Result<ResealReport> {
        let mut report = ResealReport::default();
        let Some(keyring) = self.sealer().keyring() else {
            return Ok(report);
        };
        let current = format!("enc:v1:{}:%", like_escape(keyring.active_id()));
        for (table, column) in columns {
            let changed = self.reseal_column(table, column, &current).await?;
            if changed > 0 {
                report.sealed.push((format!("{table}.{column}"), changed));
            }
        }
        report.indexed = self.reindex_push_tokens().await? + self.reindex_subscribers().await?;
        if report.total() > 0 {
            tracing::info!(sealed = ?report.sealed, indexed = report.indexed, "secrets resealed");
        }
        Ok(report)
    }

    async fn reseal_column(&self, table: &str, column: &str, current: &str) -> Result<u64> {
        let aad = format!("{table}.{column}");
        let select = self.dialect(&format!(
            r#"SELECT "id", "{column}" FROM "{table}"
               WHERE "id" > $1 AND "{column}" IS NOT NULL AND "{column}" NOT LIKE $2 ESCAPE '\'
               ORDER BY "id" LIMIT $3"#
        ));
        let update = self.dialect(&format!(
            r#"UPDATE "{table}" SET "{column}" = $1 WHERE "id" = $2 AND "{column}" = $3"#
        ));
        let (mut after, mut changed) = (i64::MIN, 0);
        loop {
            let rows = toasty::sql::query(select.clone())
                .bind(after)
                .bind(current)
                .bind(BATCH)
                .exec(&mut self.db())
                .await?;
            for value in &rows {
                let row = Row::new("reseal", value)?;
                let (id, stored) = (row.i64(0)?, row.string(1)?);
                after = id;
                if !self.sealer().needs_seal(stored) {
                    continue;
                }
                let plain = self.sealer().open(&aad, stored)?;
                changed += toasty::sql::statement(update.clone())
                    .bind(self.sealer().seal(&aad, &plain))
                    .bind(id)
                    .bind(stored)
                    .exec(&mut self.db())
                    .await?;
            }
            if rows.len() < BATCH as usize {
                return Ok(changed);
            }
        }
    }

    /// Recomputes push token indexes that do not use the active key.
    async fn reindex_push_tokens(&self) -> Result<u64> {
        let mut changed = 0;
        for record in MonitorRecord::all().exec(&mut self.db()).await? {
            let current = record.push_token_index.clone();
            let id = record.id;
            let monitor = convert::monitor(self.sealer(), record)?;
            let index = convert::push_token_index(self.sealer(), &monitor.spec.check);
            if index != current {
                MonitorRecord::update_by_id(id)
                    .push_token_index(index)
                    .exec(&mut self.db())
                    .await?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Counts stored values per sealed column that are still plaintext
    /// (diagnostics and tests).
    pub async fn plaintext_secrets(&self) -> Result<u64> {
        let mut total = 0;
        for (table, column) in SEALED_COLUMNS {
            let sql = self.dialect(&format!(
                r#"SELECT "{column}" FROM "{table}" WHERE "{column}" IS NOT NULL"#
            ));
            for value in &toasty::sql::query(sql).exec(&mut self.db()).await? {
                if !is_sealed(Row::new("plaintext_secrets", value)?.string(0)?) {
                    total += 1;
                }
            }
        }
        Ok(total)
    }

    /// `$n` placeholders as the backend expects them.
    fn dialect(&self, sql: &str) -> String {
        if self.backend() == Backend::Postgres {
            sql.to_owned()
        } else {
            sql.replace('$', "?")
        }
    }
}
