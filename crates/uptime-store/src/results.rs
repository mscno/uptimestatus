//! Persisting executed checks: raw result, runtime update and daily counters,
//! all in one transaction.

use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};
use uptime_domain::{
    Alert, AlertMonitor, DownReason, FailureKind, Health, MonitorId, MonitorState, Runtime, Tally,
    Transition, Verdict,
};

use crate::{
    Backend, Result, Store, StoreError, convert, incidents,
    models::{
        CheckResultRecord, MonitorChannelRecord, MonitorDailyRecord, MonitorRecord,
        MonitorRuntimeRecord,
    },
    notifications,
    secrets::Sealer,
};

/// Everything produced by executing one claimed slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRecord {
    pub monitor_id: MonitorId,
    pub scheduled_for: Timestamp,
    pub checked_at: Timestamp,
    pub verdict: Verdict,
    /// Runtime after the state machine ran.
    pub runtime: Runtime,
    /// What the state machine says is worth alerting about; queues one
    /// delivery per channel attached to the monitor.
    pub transition: Option<Transition>,
    /// HTTPS: the certificate seen, and whether to warn about it.
    pub cert: Option<CertUpdate>,
    /// When the monitor is due next (decided by the scheduler).
    pub next_run_at: Timestamp,
    pub region: String,
}

/// What a check learned about the target's TLS certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CertUpdate {
    pub expires_at: Timestamp,
    /// The warning threshold reached (see [`uptime_domain::cert::warning`]).
    pub warned_days: Option<u32>,
    /// Queue a CertExpiring alert with this many days left.
    pub alert_days_left: Option<i64>,
}

/// One row of raw check history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCheck {
    pub scheduled_for: Timestamp,
    pub checked_at: Timestamp,
    pub health: Health,
    pub state_after: MonitorState,
    pub latency_ms: Option<i64>,
    pub status_code: Option<u16>,
    pub error_kind: Option<FailureKind>,
    pub error: Option<String>,
    pub response_body: Option<String>,
    pub region: String,
    /// An admin excused this failure: it counts as up.
    #[serde(default)]
    pub excused: bool,
}

/// Most rows [`Store::checks_since`] returns.
pub const WINDOW_LIMIT: usize = 20_000;

/// A check reduced to what charts and windowed uptime need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowCheck {
    pub checked_at: Timestamp,
    pub state_after: MonitorState,
    pub latency_ms: Option<i64>,
}

const PRUNE_SQL: &str = r#"
DELETE FROM "check_results"
WHERE "id" IN (
    SELECT "id" FROM "check_results" WHERE "checked_at" < $1 LIMIT $2 FOR UPDATE SKIP LOCKED
)
"#;

const PRUNE_SQLITE_SQL: &str = r#"
DELETE FROM "check_results" WHERE "id" IN (
    SELECT "id" FROM "check_results" WHERE "checked_at" < ?1 LIMIT ?2
)
"#;

const UPSERT_DAILY_SQL: &str = r#"
INSERT INTO "monitor_daily" (
    "monitor_id", "day", "total", "up", "degraded", "pending", "down", "maintenance",
    "latency_sum_ms", "latency_count", "latency_max_ms"
) VALUES ($1, $2, 1, $3, $4, $5, $6, $7, $8, $9, $8)
ON CONFLICT ("monitor_id", "day") DO UPDATE SET
    "total" = "monitor_daily"."total" + 1,
    "up" = "monitor_daily"."up" + EXCLUDED."up",
    "degraded" = "monitor_daily"."degraded" + EXCLUDED."degraded",
    "pending" = "monitor_daily"."pending" + EXCLUDED."pending",
    "down" = "monitor_daily"."down" + EXCLUDED."down",
    "maintenance" = "monitor_daily"."maintenance" + EXCLUDED."maintenance",
    "latency_sum_ms" = "monitor_daily"."latency_sum_ms" + EXCLUDED."latency_sum_ms",
    "latency_count" = "monitor_daily"."latency_count" + EXCLUDED."latency_count",
    "latency_max_ms" = GREATEST("monitor_daily"."latency_max_ms", EXCLUDED."latency_max_ms")
"#;

const UPSERT_DAILY_SQLITE_SQL: &str = r#"
INSERT INTO "monitor_daily" (
    "monitor_id", "day", "total", "up", "degraded", "pending", "down", "maintenance",
    "latency_sum_ms", "latency_count", "latency_max_ms"
) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?8)
ON CONFLICT ("monitor_id", "day") DO UPDATE SET
    "total" = "monitor_daily"."total" + 1,
    "up" = "monitor_daily"."up" + EXCLUDED."up",
    "degraded" = "monitor_daily"."degraded" + EXCLUDED."degraded",
    "pending" = "monitor_daily"."pending" + EXCLUDED."pending",
    "down" = "monitor_daily"."down" + EXCLUDED."down",
    "maintenance" = "monitor_daily"."maintenance" + EXCLUDED."maintenance",
    "latency_sum_ms" = "monitor_daily"."latency_sum_ms" + EXCLUDED."latency_sum_ms",
    "latency_count" = "monitor_daily"."latency_count" + EXCLUDED."latency_count",
    "latency_max_ms" = MAX("monitor_daily"."latency_max_ms", EXCLUDED."latency_max_ms")
"#;

/// Moves excused failures (`$1` down, `$2` pending) to `up` for one day;
/// negative counts move them back.
const EXCUSE_DAILY_SQL: &str = r#"
UPDATE "monitor_daily" SET
    "down" = "down" - $1, "pending" = "pending" - $2, "up" = "up" + $1 + $2
WHERE "monitor_id" = $3 AND "day" = $4
"#;

const EXCUSE_DAILY_SQLITE_SQL: &str = r#"
UPDATE "monitor_daily" SET
    "down" = "down" - ?1, "pending" = "pending" - ?2, "up" = "up" + ?1 + ?2
WHERE "monitor_id" = ?3 AND "day" = ?4
"#;

impl Store {
    /// Records one executed check: raw history, runtime state (releasing the
    /// claim) and the day's counters, atomically.
    #[tracing::instrument(skip_all, fields(monitor = %record.monitor_id, state = %record.runtime.state))]
    pub async fn record_check(&self, record: &CheckRecord) -> Result<()> {
        let verdict = &record.verdict;
        let latency_ms = verdict.latency.map(convert::millis);
        let status_code = verdict.status_code.map(i32::from);
        let error_kind = match &verdict.reason {
            Some(DownReason::Probe { kind, .. }) => Some(kind.as_str().to_owned()),
            _ => None,
        };
        let error = verdict.reason.as_ref().map(ToString::to_string);

        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;

        let stored = toasty::create!(CheckResultRecord {
            monitor_id: record.monitor_id.0,
            scheduled_for: record.scheduled_for,
            checked_at: record.checked_at,
            health: verdict.health.as_str(),
            state_after: record.runtime.state.as_str(),
            latency_ms: latency_ms,
            status_code: status_code,
            error_kind: error_kind,
            error: error.clone(),
            response_body: self
                .sealer()
                .seal_opt(RESPONSE_BODY, verdict.response_body.as_deref()),
            region: record.region.as_str(),
        })
        .exec(&mut tx)
        .await?;

        let previous = MonitorRuntimeRecord::filter_by_monitor_id(record.monitor_id.0)
            .first()
            .exec(&mut tx)
            .await?
            .ok_or(StoreError::MonitorNotFound(record.monitor_id))?;
        // Incident lifecycle follows confirmed check state, independently of
        // alert transitions (recovery may follow maintenance or a pause).
        if matches!(
            record.runtime.state,
            MonitorState::Down | MonitorState::Pending
        ) && verdict.health == Health::Down
        {
            let monitor = MonitorRecord::filter_by_id(record.monitor_id.0)
                .get(&mut tx)
                .await?;
            incidents::record_auto_failure(&mut tx, self.sealer(), &monitor, record, stored)
                .await?;
        } else if matches!(
            record.runtime.state,
            MonitorState::Up | MonitorState::Degraded
        ) && verdict.health != Health::Down
        {
            let monitor = MonitorRecord::filter_by_id(record.monitor_id.0)
                .get(&mut tx)
                .await?;
            incidents::resolve_auto(&mut tx, self.sealer(), &monitor, stored).await?;
        }
        if let Some(transition) = record.transition {
            let monitor = MonitorRecord::filter_by_id(record.monitor_id.0)
                .first()
                .exec(&mut tx)
                .await?
                .ok_or(StoreError::MonitorNotFound(record.monitor_id))?;
            let downtime_secs = match transition {
                Transition::Recovered => previous
                    .state_changed_at
                    .map(|since| record.checked_at.duration_since(since).as_secs()),
                _ => None,
            };
            let channels: Vec<i64> = MonitorChannelRecord::filter(
                MonitorChannelRecord::fields()
                    .monitor_id()
                    .eq(record.monitor_id.0),
            )
            .exec(&mut tx)
            .await?
            .into_iter()
            .map(|link| link.channel_id)
            .collect();
            if !channels.is_empty() {
                let alert = Alert {
                    event: transition.into(),
                    monitor: AlertMonitor {
                        id: monitor.id,
                        key: monitor.key,
                        name: monitor.name,
                    },
                    state: record.runtime.state,
                    previous_state: convert::parse("monitor_runtime.state", &previous.state)?,
                    error: error.clone(),
                    latency_ms,
                    status_code: verdict.status_code,
                    at: record.checked_at,
                    downtime_secs,
                    cert_days_left: None,
                };
                notifications::enqueue(&mut tx, &alert, Some(record.monitor_id.0), &channels)
                    .await?;
            }
        }

        let state_changed = previous.state != record.runtime.state.as_str();
        let state_changed_at = if state_changed {
            Some(record.checked_at)
        } else {
            previous.state_changed_at
        };

        if let Some(cert) = &record.cert {
            MonitorRuntimeRecord::update_by_monitor_id(record.monitor_id.0)
                .cert_expires_at(Some(cert.expires_at))
                .cert_warned_days(
                    cert.warned_days
                        .map(|d| i32::try_from(d).unwrap_or(i32::MAX)),
                )
                .exec(&mut tx)
                .await?;
            if let Some(days_left) = cert.alert_days_left {
                let channels: Vec<i64> = MonitorChannelRecord::filter(
                    MonitorChannelRecord::fields()
                        .monitor_id()
                        .eq(record.monitor_id.0),
                )
                .exec(&mut tx)
                .await?
                .into_iter()
                .map(|link| link.channel_id)
                .collect();
                if !channels.is_empty() {
                    let monitor = MonitorRecord::filter_by_id(record.monitor_id.0)
                        .first()
                        .exec(&mut tx)
                        .await?
                        .ok_or(StoreError::MonitorNotFound(record.monitor_id))?;
                    let alert = Alert {
                        event: uptime_domain::AlertEvent::CertExpiring,
                        monitor: AlertMonitor {
                            id: monitor.id,
                            key: monitor.key,
                            name: monitor.name,
                        },
                        state: record.runtime.state,
                        previous_state: record.runtime.state,
                        error: None,
                        latency_ms,
                        status_code: verdict.status_code,
                        at: record.checked_at,
                        downtime_secs: None,
                        cert_days_left: Some(days_left),
                    };
                    notifications::enqueue(&mut tx, &alert, Some(record.monitor_id.0), &channels)
                        .await?;
                }
            }
        }

        MonitorRuntimeRecord::update_by_monitor_id(record.monitor_id.0)
            .state(record.runtime.state.as_str())
            .consecutive_failures(
                i32::try_from(record.runtime.consecutive_failures).unwrap_or(i32::MAX),
            )
            .next_run_at(record.next_run_at)
            .claimed_by(None::<String>)
            .last_checked_at(Some(record.checked_at))
            .last_latency_ms(latency_ms)
            .last_status_code(status_code)
            .last_error(error)
            .state_changed_at(state_changed_at)
            .exec(&mut tx)
            .await?;

        let mut tally = Tally::default();
        tally.record(record.runtime.state);
        let day = record.checked_at.to_zoned(TimeZone::UTC).date();
        let sql = if self.backend() == Backend::Postgres {
            UPSERT_DAILY_SQL
        } else {
            UPSERT_DAILY_SQLITE_SQL
        };
        toasty::sql::statement(sql)
            .bind(record.monitor_id.0)
            .bind(self.raw_date(day))
            .bind(to_i64(tally.up))
            .bind(to_i64(tally.degraded))
            .bind(to_i64(tally.pending))
            .bind(to_i64(tally.down))
            .bind(to_i64(tally.maintenance))
            .bind(latency_ms.unwrap_or(0))
            .bind(i64::from(latency_ms.is_some()))
            .exec(&mut tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// The most recent checks for a monitor, newest first.
    pub async fn recent_checks(&self, monitor: MonitorId, limit: u32) -> Result<Vec<StoredCheck>> {
        let fields = CheckResultRecord::fields();
        CheckResultRecord::filter(fields.monitor_id().eq(monitor.0))
            .order_by(fields.checked_at().desc())
            .limit(limit as usize)
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(|record| stored_check(self.sealer(), record))
            .collect()
    }

    /// The lightweight results checked at or after `since`, oldest first.
    /// Feeds the latency charts and short-window uptime; capped at
    /// [`WINDOW_LIMIT`] rows (the newest ones win).
    pub async fn checks_since(
        &self,
        monitor: MonitorId,
        since: Timestamp,
    ) -> Result<Vec<WindowCheck>> {
        let fields = CheckResultRecord::fields();
        let mut rows = CheckResultRecord::filter(
            fields
                .monitor_id()
                .eq(monitor.0)
                .and(fields.checked_at().ge(since)),
        )
        .order_by(fields.checked_at().desc())
        .limit(WINDOW_LIMIT)
        .exec(&mut self.db())
        .await?
        .into_iter()
        .map(|r| {
            Ok(WindowCheck {
                checked_at: r.checked_at,
                state_after: if r.excused_at.is_some() {
                    MonitorState::Up
                } else {
                    convert::parse("check_results.state_after", &r.state_after)?
                },
                latency_ms: r.latency_ms,
            })
        })
        .collect::<Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Deletes raw results checked before `cutoff`, `batch` rows at a time (to
    /// keep transactions short). Daily counters are kept. Returns rows deleted.
    ///
    /// Safe to run from several instances at once.
    #[tracing::instrument(skip(self))]
    pub async fn prune_checks_before(&self, cutoff: Timestamp, batch: u32) -> Result<u64> {
        let mut db = self.db();
        let mut total = 0;
        loop {
            let sql = if self.backend() == Backend::Postgres {
                PRUNE_SQL
            } else {
                PRUNE_SQLITE_SQL
            };
            let deleted = toasty::sql::statement(sql)
                .bind(self.raw_timestamp(cutoff))
                .bind(i64::from(batch.max(1)))
                .exec(&mut db)
                .await?;
            total += deleted;
            if deleted < u64::from(batch.max(1)) {
                break;
            }
        }
        if total > 0 {
            tracing::info!(deleted = total, "pruned old check results");
        }
        Ok(total)
    }

    /// Excuses the monitor's failed checks in `[from, to]`: they count as up
    /// in uptime and the daily bars, while their diagnostics stay. Returns how
    /// many checks were excused.
    #[tracing::instrument(skip(self))]
    pub async fn excuse_checks(
        &self,
        monitor: MonitorId,
        from: Timestamp,
        to: Timestamp,
        now: Timestamp,
    ) -> Result<u64> {
        self.set_excused(monitor, from, to, Some(now)).await
    }

    /// Counts the monitor's excused checks in `[from, to]` again. Returns how
    /// many checks changed.
    #[tracing::instrument(skip(self))]
    pub async fn restore_checks(
        &self,
        monitor: MonitorId,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<u64> {
        self.set_excused(monitor, from, to, None).await
    }

    async fn set_excused(
        &self,
        monitor: MonitorId,
        from: Timestamp,
        to: Timestamp,
        excused_at: Option<Timestamp>,
    ) -> Result<u64> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let fields = CheckResultRecord::fields();
        let rows = CheckResultRecord::filter(
            fields
                .monitor_id()
                .eq(monitor.0)
                .and(fields.checked_at().ge(from))
                .and(fields.checked_at().le(to))
                .and(fields.health().eq(Health::Down.as_str())),
        )
        .exec(&mut tx)
        .await?;
        // (down, pending) checks moved per UTC day.
        let mut days: std::collections::BTreeMap<Date, (i64, i64)> = Default::default();
        let mut changed = 0;
        for row in rows {
            if row.excused_at.is_some() == excused_at.is_some() {
                continue;
            }
            let moved = match convert::parse("check_results.state_after", &row.state_after)? {
                MonitorState::Down => (1, 0),
                MonitorState::Pending => (0, 1),
                _ => continue,
            };
            CheckResultRecord::update_by_id(row.id)
                .excused_at(excused_at)
                .exec(&mut tx)
                .await?;
            let day = days
                .entry(row.checked_at.to_zoned(TimeZone::UTC).date())
                .or_default();
            day.0 += moved.0;
            day.1 += moved.1;
            changed += 1;
        }
        let sign = if excused_at.is_some() { 1 } else { -1 };
        let sql = if self.backend() == Backend::Postgres {
            EXCUSE_DAILY_SQL
        } else {
            EXCUSE_DAILY_SQLITE_SQL
        };
        for (day, (down, pending)) in days {
            toasty::sql::statement(sql)
                .bind(sign * down)
                .bind(sign * pending)
                .bind(monitor.0)
                .bind(self.raw_date(day))
                .exec(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// The per-state counts for one UTC day.
    pub async fn daily_tally(&self, monitor: MonitorId, day: Date) -> Result<Option<Tally>> {
        let Some(row) = MonitorDailyRecord::filter_by_monitor_id_and_day(monitor.0, day)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        let count =
            |value: i64| u64::try_from(value).map_err(|e| StoreError::corrupt("monitor_daily", e));
        Ok(Some(Tally {
            total: count(row.total)?,
            up: count(row.up)?,
            degraded: count(row.degraded)?,
            pending: count(row.pending)?,
            down: count(row.down)?,
            maintenance: count(row.maintenance)?,
        }))
    }
}

fn to_i64(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Associated data for sealed response excerpts.
const RESPONSE_BODY: &str = "check_results.response_body";

pub(crate) fn stored_check(sealer: &Sealer, record: CheckResultRecord) -> Result<StoredCheck> {
    Ok(StoredCheck {
        scheduled_for: record.scheduled_for,
        checked_at: record.checked_at,
        health: convert::parse("check_results.health", &record.health)?,
        state_after: convert::parse("check_results.state_after", &record.state_after)?,
        latency_ms: record.latency_ms,
        status_code: record
            .status_code
            .map(|code| {
                u16::try_from(code).map_err(|e| StoreError::corrupt("check_results.status_code", e))
            })
            .transpose()?,
        error_kind: record
            .error_kind
            .as_deref()
            .map(|kind| convert::parse("check_results.error_kind", kind))
            .transpose()?,
        error: record.error,
        response_body: sealer.open_opt(RESPONSE_BODY, record.response_body.as_deref())?,
        region: record.region,
        excused: record.excused_at.is_some(),
    })
}
