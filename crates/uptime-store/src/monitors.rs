use std::collections::HashMap;

use jiff::Timestamp;
use toasty::Json;
use uptime_domain::{MonitorId, MonitorSpec, MonitorState, Runtime};

use crate::{
    Result, Store, StoreError, convert,
    models::{MonitorRecord, MonitorRuntimeRecord},
};

/// A stored monitor: its identity plus the admin-defined spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub id: MonitorId,
    pub spec: MonitorSpec,
}

/// The scheduler-owned side of a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub runtime: Runtime,
    pub next_run_at: Timestamp,
    pub scheduled_for: Option<Timestamp>,
    pub claimed_by: Option<String>,
    pub last_checked_at: Option<Timestamp>,
    pub last_latency_ms: Option<i64>,
    pub last_status_code: Option<u16>,
    pub last_error: Option<String>,
    pub state_changed_at: Option<Timestamp>,
    pub cert_expires_at: Option<Timestamp>,
}

/// A monitor together with its scheduler-owned state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorOverview {
    pub monitor: Monitor,
    pub runtime: RuntimeSnapshot,
}

impl Store {
    /// Inserts a monitor and its runtime row. The first check is due at `first_run_at`.
    #[tracing::instrument(skip_all, fields(key = %spec.key))]
    pub async fn create_monitor(
        &self,
        spec: &MonitorSpec,
        first_run_at: Timestamp,
    ) -> Result<Monitor> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;

        let existing = MonitorRecord::filter_by_key(spec.key.as_str())
            .first()
            .exec(&mut tx)
            .await?;
        if existing.is_some() {
            return Err(StoreError::DuplicateKey(spec.key.clone()));
        }

        let policy = &spec.policy;
        let record = toasty::create!(MonitorRecord {
            key: spec.key.as_str(),
            name: spec.name.as_str(),
            check: Json(spec.check.clone()),
            interval_ms: convert::millis(policy.interval),
            retry_interval_ms: convert::millis(policy.retry_interval),
            timeout_ms: convert::millis(policy.timeout),
            retries: i32::try_from(policy.retries).unwrap_or(i32::MAX),
            invert: policy.invert,
            degraded_after_ms: policy.degraded_after.map(convert::millis),
            resend_every: i32::try_from(policy.resend_every).unwrap_or(i32::MAX),
            active: spec.active,
            tags: convert::join_tags(&spec.tags),
            group_path: spec.group.clone(),
        })
        .exec(&mut tx)
        .await?;

        let initial_state = if spec.active {
            MonitorState::Unknown
        } else {
            MonitorState::Paused
        };
        toasty::create!(MonitorRuntimeRecord {
            monitor_id: record.id,
            state: initial_state.as_str(),
            consecutive_failures: 0,
            next_run_at: first_run_at,
        })
        .exec(&mut tx)
        .await?;

        tx.commit().await?;
        convert::monitor(record)
    }

    pub async fn monitor(&self, id: MonitorId) -> Result<Option<Monitor>> {
        MonitorRecord::filter_by_id(id.0)
            .first()
            .exec(&mut self.db())
            .await?
            .map(convert::monitor)
            .transpose()
    }

    /// All monitors, ordered by key.
    pub async fn list_monitors(&self) -> Result<Vec<Monitor>> {
        MonitorRecord::all()
            .order_by(MonitorRecord::fields().key().asc())
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(convert::monitor)
            .collect()
    }

    pub async fn runtime(&self, id: MonitorId) -> Result<Option<RuntimeSnapshot>> {
        MonitorRuntimeRecord::filter_by_monitor_id(id.0)
            .first()
            .exec(&mut self.db())
            .await?
            .map(convert::runtime_snapshot)
            .transpose()
    }

    /// Replaces a monitor's spec. The monitor becomes due at `now` (or paused
    /// when the new spec is inactive).
    #[tracing::instrument(skip_all, fields(monitor = %id, key = %spec.key))]
    pub async fn update_monitor(
        &self,
        id: MonitorId,
        spec: &MonitorSpec,
        now: Timestamp,
    ) -> Result<Monitor> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let existing = MonitorRecord::filter_by_id(id.0)
            .first()
            .exec(&mut tx)
            .await?
            .ok_or(StoreError::MonitorNotFound(id))?;
        if existing.key != spec.key.as_str()
            && MonitorRecord::filter_by_key(spec.key.as_str())
                .first()
                .exec(&mut tx)
                .await?
                .is_some()
        {
            return Err(StoreError::DuplicateKey(spec.key.clone()));
        }

        let policy = &spec.policy;
        MonitorRecord::update_by_id(id.0)
            .key(spec.key.as_str())
            .name(spec.name.as_str())
            .check(Json(spec.check.clone()))
            .interval_ms(convert::millis(policy.interval))
            .retry_interval_ms(convert::millis(policy.retry_interval))
            .timeout_ms(convert::millis(policy.timeout))
            .retries(i32::try_from(policy.retries).unwrap_or(i32::MAX))
            .invert(policy.invert)
            .degraded_after_ms(policy.degraded_after.map(convert::millis))
            .resend_every(i32::try_from(policy.resend_every).unwrap_or(i32::MAX))
            .active(spec.active)
            .tags(convert::join_tags(&spec.tags))
            .group_path(spec.group.clone())
            .exec(&mut tx)
            .await?;
        reschedule(&mut tx, id, spec.active, now).await?;
        tx.commit().await?;
        Ok(Monitor {
            id,
            spec: spec.clone(),
        })
    }

    /// Replaces a monitor's tags only (bulk tagging); leaves its schedule alone.
    pub async fn set_monitor_tags(&self, id: MonitorId, tags: &[String]) -> Result<()> {
        if MonitorRecord::filter_by_id(id.0)
            .first()
            .exec(&mut self.db())
            .await?
            .is_none()
        {
            return Err(StoreError::MonitorNotFound(id));
        }
        MonitorRecord::update_by_id(id.0)
            .tags(convert::join_tags(tags))
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// Pauses or resumes a monitor. Resuming makes it due at `now`.
    #[tracing::instrument(skip(self))]
    pub async fn set_monitor_active(
        &self,
        id: MonitorId,
        active: bool,
        now: Timestamp,
    ) -> Result<()> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if MonitorRecord::filter_by_id(id.0)
            .first()
            .exec(&mut tx)
            .await?
            .is_none()
        {
            return Err(StoreError::MonitorNotFound(id));
        }
        MonitorRecord::update_by_id(id.0)
            .active(active)
            .exec(&mut tx)
            .await?;
        reschedule(&mut tx, id, active, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every monitor with its runtime, ordered by key (the admin dashboard).
    pub async fn monitor_overviews(&self) -> Result<Vec<MonitorOverview>> {
        let monitors = self.list_monitors().await?;
        let mut runtimes: HashMap<i64, MonitorRuntimeRecord> = MonitorRuntimeRecord::all()
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(|runtime| (runtime.monitor_id, runtime))
            .collect();
        monitors
            .into_iter()
            .map(|monitor| {
                let runtime = runtimes.remove(&monitor.id.0).ok_or_else(|| {
                    StoreError::corrupt("monitor_runtime", format!("missing for {}", monitor.id))
                })?;
                Ok(MonitorOverview {
                    monitor,
                    runtime: convert::runtime_snapshot(runtime)?,
                })
            })
            .collect()
    }

    /// Deletes a monitor and (by cascade) its runtime, history and counters.
    /// Returns whether a monitor was deleted.
    #[tracing::instrument(skip(self))]
    pub async fn delete_monitor(&self, id: MonitorId) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "monitors" WHERE "id" = $1"#)
            .bind(id.0)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }
}

/// After a config change: active monitors are due now (a paused one starts
/// over as unknown); inactive ones are paused.
async fn reschedule(
    tx: &mut toasty::Transaction<'_>,
    id: MonitorId,
    active: bool,
    now: Timestamp,
) -> Result<()> {
    let runtime = MonitorRuntimeRecord::filter_by_monitor_id(id.0)
        .first()
        .exec(tx)
        .await?
        .ok_or(StoreError::MonitorNotFound(id))?;
    let update = MonitorRuntimeRecord::update_by_monitor_id(id.0);
    if active {
        let resumed = runtime.state == MonitorState::Paused.as_str();
        let state = if resumed {
            MonitorState::Unknown.as_str()
        } else {
            runtime.state.as_str()
        };
        update
            .state(state)
            .consecutive_failures(if resumed {
                0
            } else {
                runtime.consecutive_failures
            })
            .next_run_at(now)
            .claimed_by(None::<String>)
            .exec(tx)
            .await?;
    } else {
        update.state(MonitorState::Paused.as_str()).exec(tx).await?;
    }
    Ok(())
}
