//! The check queue. PostgreSQL uses `FOR UPDATE SKIP LOCKED`; SQLite and Turso
//! use guarded updates to claim each due slot once.

use std::collections::HashMap;

use jiff::Timestamp;
use uptime_domain::{CheckSpec, MonitorId, MonitorState, Push, Runtime};

use crate::{
    Backend, Monitor, Result, Store, StoreError,
    convert::{self, Row},
    models::{MonitorRecord, MonitorRuntimeRecord},
};

/// A monitor slot this instance now owns and must execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub monitor: Monitor,
    /// The runtime before this check (input to the state machine).
    pub runtime: Runtime,
    /// The slot being executed; the next slot is computed from it (drift-free).
    pub scheduled_for: Timestamp,
    /// Push monitors: the latest heartbeat.
    pub last_push: Option<Push>,
    /// HTTPS monitors: the certificate warning threshold already alerted about.
    pub cert_warned_days: Option<u32>,
}

/// Grace added to a claimed monitor's timeout before its slot is reclaimable.
const WATCHDOG_GRACE_MS: i64 = 60_000;

const CLAIM_DUE_SQL: &str = r#"
WITH due AS (
    SELECT r."monitor_id"
    FROM "monitor_runtime" r
    JOIN "monitors" m ON m."id" = r."monitor_id"
    WHERE m."active" AND r."next_run_at" <= $1
    ORDER BY r."next_run_at", r."monitor_id"
    LIMIT $2
    FOR UPDATE OF r SKIP LOCKED
)
UPDATE "monitor_runtime" r
SET "scheduled_for" = r."next_run_at",
    "next_run_at" = $1 + (m."timeout_ms" + $4) * INTERVAL '1 millisecond',
    "claimed_by" = $3
FROM due, "monitors" m
WHERE r."monitor_id" = due."monitor_id" AND m."id" = r."monitor_id"
RETURNING r."monitor_id", r."scheduled_for", r."state", r."consecutive_failures"
"#;

const PUSH_MONITOR_SQL: &str = r#"
SELECT "id" FROM "monitors"
WHERE "check"::jsonb->>'type' = 'push' AND "check"::jsonb->>'token' = $1
"#;

const PUSH_MONITOR_SQLITE_SQL: &str = r#"
SELECT "id" FROM "monitors"
WHERE json_extract("check", '$.type') = 'push' AND json_extract("check", '$.token') = ?1
"#;

const DUE_SQLITE_SQL: &str = r#"
SELECT r."monitor_id", r."next_run_at", m."timeout_ms"
FROM "monitor_runtime" r JOIN "monitors" m ON m."id" = r."monitor_id"
WHERE m."active" AND r."next_run_at" <= ?1
ORDER BY r."next_run_at", r."monitor_id" LIMIT ?2
"#;

const CLAIM_SQLITE_SQL: &str = r#"
UPDATE "monitor_runtime"
SET "scheduled_for" = "next_run_at", "next_run_at" = ?1, "claimed_by" = ?2
WHERE "monitor_id" = ?3 AND "next_run_at" = ?4 AND "next_run_at" <= ?5
  AND EXISTS (SELECT 1 FROM "monitors" m WHERE m."id" = "monitor_runtime"."monitor_id" AND m."active")
RETURNING "monitor_id", "scheduled_for", "state", "consecutive_failures"
"#;

impl Store {
    /// Records a heartbeat for the push monitor with `token` and makes it due
    /// now, so the push is judged at once. `None` if no monitor has the token.
    pub async fn record_push(&self, token: &str, push: &Push) -> Result<Option<MonitorId>> {
        let mut db = self.db();
        let sql = if self.backend() == Backend::Postgres {
            PUSH_MONITOR_SQL
        } else {
            PUSH_MONITOR_SQLITE_SQL
        };
        let rows = toasty::sql::query(sql).bind(token).exec(&mut db).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let id = Row::new("record_push", row)?.i64(0)?;
        MonitorRuntimeRecord::update_by_monitor_id(id)
            .last_push_at(Some(push.at))
            .last_push_up(Some(push.up))
            .last_push_message(push.message.clone())
            .last_push_ms(push.ping.map(convert::millis))
            .next_run_at(push.at)
            .exec(&mut db)
            .await?;
        Ok(Some(MonitorId(id)))
    }

    /// Claims up to `limit` active monitors whose `next_run_at <= now`, oldest first.
    ///
    /// Claiming moves `next_run_at` to `now + timeout + 60s`, so a claim that
    /// is never completed (crash, deploy) becomes due again on its own.
    #[tracing::instrument(skip(self))]
    pub async fn claim_due(&self, now: Timestamp, limit: u32, claimer: &str) -> Result<Vec<Claim>> {
        let mut db = self.db();
        let rows = if self.backend() == Backend::Postgres {
            toasty::sql::query(CLAIM_DUE_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .bind(claimer)
                .bind(WATCHDOG_GRACE_MS)
                .exec(&mut db)
                .await?
        } else {
            let candidates = toasty::sql::query(DUE_SQLITE_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .exec(&mut db)
                .await?;
            let mut rows = Vec::new();
            for candidate in &candidates {
                let row = Row::new("claim_due.candidate", candidate)?;
                let (id, scheduled_for, timeout_ms) = (row.i64(0)?, row.timestamp(1)?, row.i64(2)?);
                let lease_ms = timeout_ms.saturating_add(WATCHDOG_GRACE_MS);
                let lease = now
                    .checked_add(std::time::Duration::from_millis(lease_ms.max(0) as u64))
                    .map_err(|e| StoreError::corrupt("claim_due.lease", e))?;
                rows.extend(
                    toasty::sql::query(CLAIM_SQLITE_SQL)
                        .bind(self.raw_timestamp(lease))
                        .bind(claimer)
                        .bind(id)
                        .bind(self.raw_timestamp(scheduled_for))
                        .bind(self.raw_timestamp(now))
                        .exec(&mut db)
                        .await?,
                );
            }
            rows
        };
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut slots = rows
            .iter()
            .map(|value| {
                let row = Row::new("claim_due", value)?;
                let runtime = Runtime {
                    state: convert::parse::<MonitorState>("claim_due.state", row.string(2)?)?,
                    consecutive_failures: u32::try_from(row.i64(3)?)
                        .map_err(|e| StoreError::corrupt("claim_due.consecutive_failures", e))?,
                };
                Ok((row.i64(0)?, row.timestamp(1)?, runtime))
            })
            .collect::<Result<Vec<_>>>()?;
        slots.sort_by_key(|(id, scheduled_for, _)| (*scheduled_for, *id));

        let ids: Vec<i64> = slots.iter().map(|(id, ..)| *id).collect();
        let mut monitors: HashMap<i64, Monitor> =
            MonitorRecord::filter(MonitorRecord::fields().id().in_list(ids))
                .exec(&mut db)
                .await?
                .into_iter()
                .map(|record| convert::monitor(record).map(|m| (m.id.0, m)))
                .collect::<Result<_>>()?;

        let runtime_ids: Vec<i64> = monitors.keys().copied().collect();
        let mut extras: HashMap<i64, (Option<Push>, Option<u32>)> = MonitorRuntimeRecord::filter(
            MonitorRuntimeRecord::fields()
                .monitor_id()
                .in_list(runtime_ids),
        )
        .exec(&mut db)
        .await?
        .into_iter()
        .map(|r| {
            let push = r.last_push_at.map(|at| Push {
                at,
                up: r.last_push_up.unwrap_or(true),
                message: r.last_push_message,
                ping: r
                    .last_push_ms
                    .and_then(|ms| u64::try_from(ms).ok())
                    .map(std::time::Duration::from_millis),
            });
            let warned = r.cert_warned_days.and_then(|d| u32::try_from(d).ok());
            (r.monitor_id, (push, warned))
        })
        .collect();

        slots
            .into_iter()
            .map(|(id, scheduled_for, runtime)| {
                let monitor = monitors
                    .remove(&id)
                    .ok_or_else(|| StoreError::corrupt("claim_due", "monitor vanished"))?;
                let (last_push, cert_warned_days) = extras.remove(&id).unwrap_or_default();
                let last_push =
                    last_push.filter(|_| matches!(monitor.spec.check, CheckSpec::Push(_)));
                Ok(Claim {
                    monitor,
                    runtime,
                    scheduled_for,
                    last_push,
                    cert_warned_days,
                })
            })
            .collect()
    }
}
