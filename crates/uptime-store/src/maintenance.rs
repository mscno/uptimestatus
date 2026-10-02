//! Maintenance windows and the monitors they cover.

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use uptime_domain::{MaintenanceSpec, MonitorId, MonitorKey, Repeat};

use crate::{
    Result, Store, StoreError, convert,
    models::{MaintenanceMonitorRecord, MaintenanceRecord, MonitorRecord},
    pages::resolve_keys,
};

/// A stored window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Maintenance {
    pub id: i64,
    pub spec: MaintenanceSpec,
    pub monitor_ids: Vec<MonitorId>,
}

const ACTIVE_MONITORS_SQL: &str = r#"
SELECT DISTINCT mm."monitor_id"
FROM "maintenance_monitors" mm
JOIN "maintenances" m ON m."id" = mm."maintenance_id"
WHERE m."repeat" IS NULL AND m."starts_at" <= $1 AND m."ends_at" > $1
"#;

impl Store {
    pub async fn create_maintenance(
        &self,
        spec: &MaintenanceSpec,
        now: Timestamp,
    ) -> Result<Maintenance> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let ids = resolve_keys(&mut tx, &spec.monitors).await?;
        let record = toasty::create!(MaintenanceRecord {
            title: spec.title.trim(),
            description: spec.description.clone(),
            starts_at: spec.starts_at,
            ends_at: spec.ends_at,
            repeat: spec.repeat.map(|r| r.as_str().to_owned()),
            repeat_until: spec.repeat_until,
            created_at: now,
        })
        .exec(&mut tx)
        .await?;
        link(&mut tx, record.id, &ids).await?;
        tx.commit().await?;
        self.maintenance(record.id)
            .await?
            .ok_or(StoreError::MaintenanceNotFound(record.id))
    }

    pub async fn update_maintenance(&self, id: i64, spec: &MaintenanceSpec) -> Result<Maintenance> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if MaintenanceRecord::filter_by_id(id)
            .first()
            .exec(&mut tx)
            .await?
            .is_none()
        {
            return Err(StoreError::MaintenanceNotFound(id));
        }
        let ids = resolve_keys(&mut tx, &spec.monitors).await?;
        MaintenanceRecord::update_by_id(id)
            .title(spec.title.trim())
            .description(spec.description.clone())
            .starts_at(spec.starts_at)
            .ends_at(spec.ends_at)
            .repeat(spec.repeat.map(|r| r.as_str().to_owned()))
            .repeat_until(spec.repeat_until)
            .exec(&mut tx)
            .await?;
        toasty::sql::statement(r#"DELETE FROM "maintenance_monitors" WHERE "maintenance_id" = $1"#)
            .bind(id)
            .exec(&mut tx)
            .await?;
        link(&mut tx, id, &ids).await?;
        tx.commit().await?;
        self.maintenance(id)
            .await?
            .ok_or(StoreError::MaintenanceNotFound(id))
    }

    pub async fn delete_maintenance(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "maintenances" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    pub async fn maintenance(&self, id: i64) -> Result<Option<Maintenance>> {
        let Some(record) = MaintenanceRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        Ok(self.with_monitors(vec![record]).await?.pop())
    }

    /// Every window, latest start first.
    pub async fn list_maintenances(&self) -> Result<Vec<Maintenance>> {
        let records = MaintenanceRecord::all()
            .order_by(MaintenanceRecord::fields().starts_at().desc())
            .exec(&mut self.db())
            .await?;
        self.with_monitors(records).await
    }

    /// Windows overlapping `[from, to)`, soonest first. Each occurrence of a
    /// recurring window is its own entry, with its own start and end.
    pub async fn maintenances_between(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Maintenance>> {
        let fields = MaintenanceRecord::fields();
        let once = MaintenanceRecord::filter(
            fields
                .starts_at()
                .lt(to)
                .and(fields.ends_at().gt(from))
                .and(fields.repeat().is_none()),
        )
        .exec(&mut self.db())
        .await?;
        let mut windows = self.with_monitors(once).await?;
        for window in self.recurring().await? {
            for (starts_at, ends_at) in window.spec.occurrences(from, to) {
                let mut occurrence = window.clone();
                occurrence.spec.starts_at = starts_at;
                occurrence.spec.ends_at = ends_at;
                windows.push(occurrence);
            }
        }
        windows.sort_by_key(|w| (w.spec.starts_at, w.id));
        Ok(windows)
    }

    /// Monitors inside an active window at `at`.
    pub async fn monitors_in_maintenance(&self, at: Timestamp) -> Result<HashSet<MonitorId>> {
        let rows = toasty::sql::query(ACTIVE_MONITORS_SQL)
            .bind(self.raw_timestamp(at))
            .exec(&mut self.db())
            .await?;
        let mut monitors: HashSet<MonitorId> = rows
            .iter()
            .map(|row| {
                Ok(MonitorId(
                    convert::Row::new("monitors_in_maintenance", row)?.i64(0)?,
                ))
            })
            .collect::<Result<_>>()?;
        for window in self.recurring().await? {
            if window.spec.is_active(at) {
                monitors.extend(window.monitor_ids);
            }
        }
        Ok(monitors)
    }

    /// The recurring windows (few; their occurrences are computed).
    async fn recurring(&self) -> Result<Vec<Maintenance>> {
        let records = MaintenanceRecord::filter(MaintenanceRecord::fields().repeat().is_some())
            .exec(&mut self.db())
            .await?;
        self.with_monitors(records).await
    }

    async fn with_monitors(&self, records: Vec<MaintenanceRecord>) -> Result<Vec<Maintenance>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let mut db = self.db();
        let ids: Vec<i64> = records.iter().map(|r| r.id).collect();
        let links = MaintenanceMonitorRecord::filter(
            MaintenanceMonitorRecord::fields()
                .maintenance_id()
                .in_list(ids),
        )
        .exec(&mut db)
        .await?;
        let monitor_ids: Vec<i64> = links.iter().map(|l| l.monitor_id).collect();
        let keys: HashMap<i64, MonitorKey> = if monitor_ids.is_empty() {
            HashMap::new()
        } else {
            MonitorRecord::filter(MonitorRecord::fields().id().in_list(monitor_ids))
                .exec(&mut db)
                .await?
                .into_iter()
                .map(|m| Ok((m.id, convert::parse::<MonitorKey>("monitors.key", &m.key)?)))
                .collect::<Result<_>>()?
        };
        let mut by_window: HashMap<i64, Vec<i64>> = HashMap::new();
        for link in links {
            by_window
                .entry(link.maintenance_id)
                .or_default()
                .push(link.monitor_id);
        }
        records
            .into_iter()
            .map(|record| {
                let mut monitor_ids = by_window.remove(&record.id).unwrap_or_default();
                monitor_ids.sort_unstable();
                let mut monitors: Vec<MonitorKey> = monitor_ids
                    .iter()
                    .filter_map(|id| keys.get(id).cloned())
                    .collect();
                monitors.sort();
                let repeat = record
                    .repeat
                    .as_deref()
                    .map(|text| convert::parse::<Repeat>("maintenances.repeat", text))
                    .transpose()?;
                Ok(Maintenance {
                    id: record.id,
                    spec: MaintenanceSpec {
                        title: record.title,
                        description: record.description,
                        starts_at: record.starts_at,
                        ends_at: record.ends_at,
                        repeat,
                        repeat_until: record.repeat_until,
                        monitors,
                    },
                    monitor_ids: monitor_ids.into_iter().map(MonitorId).collect(),
                })
            })
            .collect()
    }
}

async fn link(
    tx: &mut toasty::Transaction<'_>,
    maintenance_id: i64,
    ids: &HashMap<MonitorKey, i64>,
) -> Result<()> {
    let mut monitor_ids: Vec<i64> = ids.values().copied().collect();
    monitor_ids.sort_unstable();
    monitor_ids.dedup();
    for monitor_id in monitor_ids {
        toasty::create!(MaintenanceMonitorRecord {
            maintenance_id: maintenance_id,
            monitor_id: monitor_id,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}
