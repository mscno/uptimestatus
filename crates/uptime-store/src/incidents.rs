//! Incidents and their timelines.

use std::collections::HashMap;

use jiff::Timestamp;
use uptime_domain::{Impact, IncidentKind, IncidentStatus, MonitorId, MonitorKey, format_duration};

use crate::{
    Result, Store, StoreError, convert,
    models::{
        CheckResultRecord, IncidentMonitorRecord, IncidentRecord, IncidentUpdateRecord,
        MonitorRecord,
    },
    pages::resolve_keys,
    results::{CheckRecord, StoredCheck, stored_check},
};

/// What an admin declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIncident {
    pub title: String,
    pub impact: Impact,
    pub status: IncidentStatus,
    /// The first update.
    pub message: String,
    pub monitors: Vec<MonitorKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncidentUpdate {
    pub id: i64,
    pub status: IncidentStatus,
    pub body: String,
    pub created_at: Timestamp,
    /// Check diagnostics, preserved even after raw results are pruned.
    pub check: Option<StoredCheck>,
}

/// An incident with its affected monitors and timeline (newest update first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incident {
    pub id: i64,
    pub title: String,
    pub impact: Impact,
    pub status: IncidentStatus,
    pub kind: IncidentKind,
    pub started_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
    pub monitor_ids: Vec<MonitorId>,
    pub updates: Vec<IncidentUpdate>,
}

impl Store {
    pub async fn declare_incident(
        &self,
        incident: &NewIncident,
        now: Timestamp,
    ) -> Result<Incident> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let ids = resolve_keys(&mut tx, &incident.monitors).await?;
        let resolved = incident.status == IncidentStatus::Resolved;
        let record = toasty::create!(IncidentRecord {
            title: incident.title.trim(),
            impact: incident.impact.as_str(),
            status: incident.status.as_str(),
            kind: IncidentKind::Manual.as_str(),
            started_at: now,
            resolved_at: resolved.then_some(now),
        })
        .exec(&mut tx)
        .await?;
        add_update(
            &mut tx,
            record.id,
            incident.status,
            incident.message.trim(),
            now,
        )
        .await?;
        let mut monitor_ids: Vec<i64> = ids.into_values().collect();
        monitor_ids.sort_unstable();
        monitor_ids.dedup();
        for monitor_id in monitor_ids {
            toasty::create!(IncidentMonitorRecord {
                incident_id: record.id,
                monitor_id: monitor_id,
            })
            .exec(&mut tx)
            .await?;
        }
        tx.commit().await?;
        self.incident(record.id)
            .await?
            .ok_or(StoreError::IncidentNotFound(record.id))
    }

    /// Adds to the timeline and moves the incident to `status` (resolving or
    /// reopening it).
    pub async fn post_incident_update(
        &self,
        id: i64,
        status: IncidentStatus,
        body: &str,
        now: Timestamp,
    ) -> Result<Incident> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let Some(current) = IncidentRecord::filter_by_id(id)
            .first()
            .exec(&mut tx)
            .await?
        else {
            return Err(StoreError::IncidentNotFound(id));
        };
        let resolved_at = match status {
            IncidentStatus::Resolved => Some(current.resolved_at.unwrap_or(now)),
            _ => None,
        };
        IncidentRecord::update_by_id(id)
            .status(status.as_str())
            .resolved_at(resolved_at)
            .exec(&mut tx)
            .await?;
        add_update(&mut tx, id, status, body.trim(), now).await?;
        tx.commit().await?;
        self.incident(id)
            .await?
            .ok_or(StoreError::IncidentNotFound(id))
    }

    pub async fn delete_incident(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "incidents" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    /// Public detail with internal check diagnostics removed.
    pub async fn public_incident(&self, id: i64) -> Result<Option<Incident>> {
        let Some(record) = IncidentRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        Ok(self.public(vec![record]).await?.pop())
    }

    pub async fn incident(&self, id: i64) -> Result<Option<Incident>> {
        let Some(record) = IncidentRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        Ok(self.hydrate(vec![record]).await?.pop())
    }

    /// The most recent incidents, open ones first.
    pub async fn list_incidents(&self, limit: u32) -> Result<Vec<Incident>> {
        let fields = IncidentRecord::fields();
        let records = IncidentRecord::all()
            .order_by(fields.started_at().desc())
            .limit(limit as usize)
            .exec(&mut self.db())
            .await?;
        let mut incidents = self.hydrate(records).await?;
        incidents.sort_by_key(|i| (i.resolved_at.is_some(), std::cmp::Reverse(i.started_at)));
        Ok(incidents)
    }

    /// Incidents touching any of `monitors` that are open, or were
    /// resolved after `resolved_since`; newest first.
    pub async fn public_incidents(
        &self,
        monitors: &[MonitorId],
        resolved_since: Timestamp,
    ) -> Result<Vec<Incident>> {
        let ids = self.incident_ids_for(monitors).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records = IncidentRecord::filter(IncidentRecord::fields().id().in_list(ids))
            .exec(&mut self.db())
            .await?
            .into_iter()
            .filter(|r| r.resolved_at.is_none_or(|at| at > resolved_since))
            .collect();
        self.public(records).await
    }

    /// Incidents touching any of `monitors` that started in
    /// `[from, to)`; newest first. A status page's incident history.
    pub async fn incident_history(
        &self,
        monitors: &[MonitorId],
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Incident>> {
        let ids = self.incident_ids_for(monitors).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let fields = IncidentRecord::fields();
        let records = IncidentRecord::filter(
            fields
                .id()
                .in_list(ids)
                .and(fields.started_at().ge(from))
                .and(fields.started_at().lt(to)),
        )
        .exec(&mut self.db())
        .await?;
        self.public(records).await
    }

    /// Incidents linked to any of `monitors`.
    async fn incident_ids_for(&self, monitors: &[MonitorId]) -> Result<Vec<i64>> {
        if monitors.is_empty() {
            return Ok(Vec::new());
        }
        let monitor_ids: Vec<i64> = monitors.iter().map(|m| m.0).collect();
        let mut ids: Vec<i64> = IncidentMonitorRecord::filter(
            IncidentMonitorRecord::fields()
                .monitor_id()
                .in_list(monitor_ids),
        )
        .exec(&mut self.db())
        .await?
        .into_iter()
        .map(|link| link.incident_id)
        .collect();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Public timelines omit internal check diagnostics, newest first.
    async fn public(&self, records: Vec<IncidentRecord>) -> Result<Vec<Incident>> {
        let mut incidents = self.hydrate(records).await?;
        for incident in &mut incidents {
            incident.updates.retain(|update| update.check.is_none());
        }
        incidents.sort_by_key(|i| std::cmp::Reverse((i.started_at, i.id)));
        Ok(incidents)
    }

    async fn hydrate(&self, records: Vec<IncidentRecord>) -> Result<Vec<Incident>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let mut db = self.db();
        let ids: Vec<i64> = records.iter().map(|r| r.id).collect();
        let mut updates: HashMap<i64, Vec<IncidentUpdate>> = HashMap::new();
        for record in IncidentUpdateRecord::filter(
            IncidentUpdateRecord::fields()
                .incident_id()
                .in_list(ids.clone()),
        )
        .exec(&mut db)
        .await?
        {
            updates
                .entry(record.incident_id)
                .or_default()
                .push(IncidentUpdate {
                    id: record.id,
                    status: convert::parse("incident_updates.status", &record.status)?,
                    body: record.body,
                    check: record
                        .check_json
                        .as_deref()
                        .map(|json| {
                            serde_json::from_str(json)
                                .map_err(|e| StoreError::corrupt("incident_updates.check_json", e))
                        })
                        .transpose()?,
                    created_at: record.created_at,
                });
        }
        let mut monitors: HashMap<i64, Vec<MonitorId>> = HashMap::new();
        for link in IncidentMonitorRecord::filter(
            IncidentMonitorRecord::fields().incident_id().in_list(ids),
        )
        .exec(&mut db)
        .await?
        {
            monitors
                .entry(link.incident_id)
                .or_default()
                .push(MonitorId(link.monitor_id));
        }
        records
            .into_iter()
            .map(|record| {
                let mut timeline = updates.remove(&record.id).unwrap_or_default();
                timeline.sort_by_key(|u| std::cmp::Reverse((u.created_at, u.id)));
                let mut monitor_ids = monitors.remove(&record.id).unwrap_or_default();
                monitor_ids.sort_unstable();
                Ok(Incident {
                    id: record.id,
                    title: record.title,
                    impact: convert::parse("incidents.impact", &record.impact)?,
                    status: convert::parse("incidents.status", &record.status)?,
                    kind: convert::parse("incidents.kind", &record.kind)?,
                    started_at: record.started_at,
                    resolved_at: record.resolved_at,
                    monitor_ids,
                    updates: timeline,
                })
            })
            .collect()
    }
}

async fn add_update(
    tx: &mut toasty::Transaction<'_>,
    incident_id: i64,
    status: IncidentStatus,
    body: &str,
    now: Timestamp,
) -> Result<()> {
    toasty::create!(IncidentUpdateRecord {
        incident_id: incident_id,
        status: status.as_str(),
        body: body,
        created_at: now,
    })
    .exec(tx)
    .await?;
    Ok(())
}

/// Opens after confirmation and captures the whole failure streak, including
/// pending attempts. Subsequent failures append diagnostics to the same incident.
pub(crate) async fn record_auto_failure(
    tx: &mut toasty::Transaction<'_>,
    monitor: &MonitorRecord,
    check: &CheckRecord,
    result: CheckResultRecord,
) -> Result<()> {
    if let Some(incident) = open_auto_incident(tx, monitor.id).await? {
        return add_check_update(
            tx,
            incident.id,
            convert::parse("incidents.status", &incident.status)?,
            result,
        )
        .await;
    }
    if check.runtime.state != uptime_domain::MonitorState::Down
        || check.runtime.consecutive_failures < 2
    {
        return Ok(());
    }
    let at = check.checked_at;
    let record = toasty::create!(IncidentRecord {
        title: format!("{} is down", monitor.name),
        impact: Impact::Major.as_str(),
        status: IncidentStatus::Investigating.as_str(),
        kind: IncidentKind::Auto.as_str(),
        monitor_id: Some(monitor.id),
        started_at: at,
    })
    .exec(&mut *tx)
    .await?;
    add_update(
        tx,
        record.id,
        IncidentStatus::Investigating,
        &format!("{} failed its checks.", monitor.name),
        at,
    )
    .await?;
    let fields = CheckResultRecord::fields();
    let mut failures = CheckResultRecord::filter(fields.monitor_id().eq(monitor.id))
        .order_by(fields.id().desc())
        .limit(check.runtime.consecutive_failures as usize)
        .exec(&mut *tx)
        .await?;
    failures.reverse();
    for failure in failures {
        if failure.health == "down" {
            add_check_update(tx, record.id, IncidentStatus::Investigating, failure).await?;
        }
    }
    toasty::create!(IncidentMonitorRecord {
        incident_id: record.id,
        monitor_id: monitor.id,
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

async fn add_check_update(
    tx: &mut toasty::Transaction<'_>,
    incident_id: i64,
    status: IncidentStatus,
    result: CheckResultRecord,
) -> Result<()> {
    let check = stored_check(result)?;
    let body = check.error.as_deref().unwrap_or("Check failed.");
    let json = serde_json::to_string(&check)
        .map_err(|e| StoreError::corrupt("incident_updates.check_json", e))?;
    toasty::create!(IncidentUpdateRecord {
        incident_id: incident_id,
        status: status.as_str(),
        body: body,
        check_json: Some(json),
        created_at: check.checked_at,
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

/// Resolves the monitor's open automatic incident, if any.
pub(crate) async fn resolve_auto(
    tx: &mut toasty::Transaction<'_>,
    monitor_id: i64,
    name: &str,
    at: Timestamp,
) -> Result<()> {
    let Some(incident) = open_auto_incident(tx, monitor_id).await? else {
        return Ok(());
    };
    IncidentRecord::update_by_id(incident.id)
        .status(IncidentStatus::Resolved.as_str())
        .resolved_at(Some(at))
        .exec(&mut *tx)
        .await?;
    let secs = at.duration_since(incident.started_at).as_secs();
    let body = format!("{name} recovered after {}.", format_duration(secs));
    add_update(tx, incident.id, IncidentStatus::Resolved, &body, at).await
}

async fn open_auto_incident(
    tx: &mut toasty::Transaction<'_>,
    monitor_id: i64,
) -> Result<Option<IncidentRecord>> {
    let fields = IncidentRecord::fields();
    Ok(IncidentRecord::filter(
        fields
            .monitor_id()
            .eq(Some(monitor_id))
            .and(fields.kind().eq(IncidentKind::Auto.as_str()))
            .and(fields.resolved_at().is_none()),
    )
    .first()
    .exec(&mut *tx)
    .await?)
}
