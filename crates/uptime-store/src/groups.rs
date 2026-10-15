//! Group monitors: who belongs to them, keeping them acyclic, and the
//! scheduling hooks that keep them current.

use jiff::Timestamp;
use uptime_domain::{CheckSpec, Member, MonitorId, MonitorSpec, MonitorState, group};

use crate::{
    Backend, Monitor, MonitorOverview, Result, Store, StoreError, models::MonitorRuntimeRecord,
};

/// Makes an idle group due at `$1` (claimed or already due ones are left alone).
const EXPEDITE_SQL: &str = r#"
UPDATE "monitor_runtime" SET "next_run_at" = $1
WHERE "monitor_id" = $2 AND "claimed_by" IS NULL AND "next_run_at" > $1
"#;

const EXPEDITE_SQLITE_SQL: &str = r#"
UPDATE "monitor_runtime" SET "next_run_at" = ?1
WHERE "monitor_id" = ?2 AND "claimed_by" IS NULL AND "next_run_at" > ?1
"#;

impl Monitor {
    /// How group evaluation sees this monitor in `state`.
    pub fn as_member(&self, state: MonitorState) -> Member<'_> {
        let spec = &self.spec;
        Member {
            key: spec.key.as_str(),
            name: spec.name.as_str(),
            group: spec.group.as_deref(),
            summarises: match &spec.check {
                CheckSpec::Group(check) => Some(check),
                _ => None,
            },
            state,
        }
    }
}

impl MonitorOverview {
    /// How group evaluation sees this monitor now.
    pub fn as_member(&self) -> Member<'_> {
        self.monitor.as_member(self.runtime.runtime.state)
    }
}

impl Store {
    /// The members of group monitor `id` with their runtime, ordered by key
    /// (empty for other monitors).
    pub async fn group_members(&self, id: MonitorId) -> Result<Vec<MonitorOverview>> {
        let overviews = self.monitor_overviews().await?;
        let all: Vec<Member<'_>> = overviews.iter().map(MonitorOverview::as_member).collect();
        let Some(of) = overviews
            .iter()
            .position(|o| o.monitor.id == id)
            .map(|at| all[at])
        else {
            return Err(StoreError::MonitorNotFound(id));
        };
        let keys: Vec<&str> = group::members(&of, &all).iter().map(|m| m.key).collect();
        Ok(overviews
            .iter()
            .filter(|o| keys.contains(&o.monitor.spec.key.as_str()))
            .cloned()
            .collect())
    }

    /// Refuses a group monitor spec (new, or replacing monitor `id`) that would
    /// make group monitors include each other.
    pub(crate) async fn ensure_acyclic(
        &self,
        id: Option<MonitorId>,
        spec: &MonitorSpec,
    ) -> Result<()> {
        if !matches!(spec.check, CheckSpec::Group(_)) {
            return Ok(());
        }
        let mut monitors = self.list_monitors().await?;
        monitors.retain(|m| Some(m.id) != id && m.spec.key != spec.key);
        let candidate = Monitor {
            id: id.unwrap_or(MonitorId(0)),
            spec: spec.clone(),
        };
        monitors.push(candidate);
        let all: Vec<Member<'_>> = monitors
            .iter()
            .map(|m| m.as_member(MonitorState::Unknown))
            .collect();
        match group::find_cycle(&all) {
            Some(keys) => Err(StoreError::GroupCycle(keys)),
            None => Ok(()),
        }
    }

    /// Releases a claimed group monitor that had nothing to judge: UNKNOWN,
    /// with `message` as its last error, and no check recorded (so uptime is
    /// unaffected).
    pub async fn record_unknown(
        &self,
        id: MonitorId,
        checked_at: Timestamp,
        next_run_at: Timestamp,
        message: &str,
    ) -> Result<()> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let previous = MonitorRuntimeRecord::filter_by_monitor_id(id.0)
            .first()
            .exec(&mut tx)
            .await?
            .ok_or(StoreError::MonitorNotFound(id))?;
        let unknown = MonitorState::Unknown.as_str();
        let state_changed_at = if previous.state == unknown {
            previous.state_changed_at
        } else {
            Some(checked_at)
        };
        MonitorRuntimeRecord::update_by_monitor_id(id.0)
            .state(unknown)
            .consecutive_failures(0)
            .next_run_at(next_run_at)
            .claimed_by(None::<String>)
            .last_checked_at(Some(checked_at))
            .last_latency_ms(None::<i64>)
            .last_status_code(None::<i32>)
            .last_error(Some(message.to_owned()))
            .state_changed_at(state_changed_at)
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Makes the group monitors that include monitor `id` due at `now`, so
    /// they re-judge promptly after it changed state. Groups being checked
    /// right now are skipped. Returns how many were made due.
    pub async fn expedite_groups(&self, id: MonitorId, now: Timestamp) -> Result<u64> {
        let monitors = self.list_monitors().await?;
        let Some(changed) = monitors.iter().find(|m| m.id == id) else {
            return Ok(0);
        };
        let folder = changed.spec.group.as_deref();
        let sql = if self.backend() == Backend::Postgres {
            EXPEDITE_SQL
        } else {
            EXPEDITE_SQLITE_SQL
        };
        let mut db = self.db();
        let mut expedited = 0;
        for group in &monitors {
            let CheckSpec::Group(check) = &group.spec.check else {
                continue;
            };
            if group.id == id || !group.spec.active || !check.covers(folder) {
                continue;
            }
            expedited += toasty::sql::statement(sql)
                .bind(self.raw_timestamp(now))
                .bind(group.id.0)
                .exec(&mut db)
                .await?;
        }
        Ok(expedited)
    }
}
