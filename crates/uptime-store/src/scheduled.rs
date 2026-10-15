//! Incident declarations and updates scheduled for later. The worker
//! publishes due ones through the same path as a manual update; publishing
//! deletes the schedule in the transaction that changes the incident
//! ([`take_scheduled`]), so concurrent publishers cannot publish twice.

use jiff::Timestamp;
use uptime_domain::{Impact, IncidentStatus, MonitorKey};

use crate::{
    Result, Store, StoreError, convert,
    models::{IncidentRecord, ScheduledUpdateRecord},
    pages::resolve_keys,
};

/// What a scheduled item does when published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduledKind {
    /// Declares a new incident.
    Declare {
        title: String,
        impact: Impact,
        monitors: Vec<MonitorKey>,
    },
    /// Posts an update to an existing incident.
    Update { incident_id: i64 },
}

/// What to schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewScheduledUpdate {
    pub publish_at: Timestamp,
    pub status: IncidentStatus,
    /// The update's message (a declaration's first update).
    pub body: String,
    pub kind: ScheduledKind,
    /// Who scheduled it, as the audit trail names them.
    pub actor: String,
}

/// A scheduled declaration or update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledUpdate {
    pub id: i64,
    pub publish_at: Timestamp,
    pub status: IncidentStatus,
    pub body: String,
    pub kind: ScheduledKind,
    pub actor: String,
    /// Why the last attempt to publish failed; it waits until edited.
    pub error: Option<String>,
    pub created_at: Timestamp,
}

impl ScheduledUpdate {
    pub fn incident_id(&self) -> Option<i64> {
        match self.kind {
            ScheduledKind::Update { incident_id } => Some(incident_id),
            ScheduledKind::Declare { .. } => None,
        }
    }
}

impl Store {
    /// Schedules a declaration (its monitors must exist) or an update (its
    /// incident must exist).
    pub async fn schedule_update(&self, new: &NewScheduledUpdate) -> Result<ScheduledUpdate> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let (incident_id, title, impact, monitors) = match &new.kind {
            ScheduledKind::Declare {
                title,
                impact,
                monitors,
            } => {
                resolve_keys(&mut tx, monitors).await?;
                let keys: Vec<String> = monitors.iter().map(ToString::to_string).collect();
                let keys = serde_json::to_string(&keys)
                    .map_err(|e| StoreError::corrupt("scheduled_incident_updates.monitors", e))?;
                (
                    None,
                    Some(title.trim().to_owned()),
                    Some(impact.as_str().to_owned()),
                    Some(keys),
                )
            }
            ScheduledKind::Update { incident_id } => {
                if IncidentRecord::filter_by_id(*incident_id)
                    .first()
                    .exec(&mut tx)
                    .await?
                    .is_none()
                {
                    return Err(StoreError::IncidentNotFound(*incident_id));
                }
                (Some(*incident_id), None, None, None)
            }
        };
        let record = toasty::create!(ScheduledUpdateRecord {
            incident_id: incident_id,
            publish_at: new.publish_at,
            status: new.status.as_str(),
            body: new.body.trim(),
            title: title,
            impact: impact,
            monitors: monitors,
            actor: new.actor.as_str(),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        // Read it back: the database keeps `created_at` at its own precision
        // (microseconds on PostgreSQL), not the nanoseconds set on insert.
        self.scheduled(record.id)
            .await?
            .ok_or(StoreError::ScheduledNotFound(record.id))
    }

    pub async fn scheduled(&self, id: i64) -> Result<Option<ScheduledUpdate>> {
        ScheduledUpdateRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
            .map(scheduled)
            .transpose()
    }

    /// An incident's scheduled updates, soonest first.
    pub async fn scheduled_for_incident(&self, incident_id: i64) -> Result<Vec<ScheduledUpdate>> {
        let records = ScheduledUpdateRecord::filter(
            ScheduledUpdateRecord::fields()
                .incident_id()
                .eq(Some(incident_id)),
        )
        .exec(&mut self.db())
        .await?;
        sorted(records)
    }

    /// Scheduled declarations of new incidents, soonest first.
    pub async fn scheduled_declarations(&self) -> Result<Vec<ScheduledUpdate>> {
        let records =
            ScheduledUpdateRecord::filter(ScheduledUpdateRecord::fields().incident_id().is_none())
                .exec(&mut self.db())
                .await?;
        sorted(records)
    }

    /// Items due at `now` that have not failed, soonest first.
    pub async fn due_scheduled(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ScheduledUpdate>> {
        let fields = ScheduledUpdateRecord::fields();
        let records = ScheduledUpdateRecord::filter(
            fields.publish_at().le(now).and(fields.error().is_none()),
        )
        .order_by(fields.publish_at().asc())
        .limit(limit)
        .exec(&mut self.db())
        .await?;
        sorted(records)
    }

    /// Changes when and what an item publishes, clearing a failure. For a
    /// declaration, `declaration` also changes its title and impact.
    pub async fn reschedule(
        &self,
        id: i64,
        publish_at: Timestamp,
        status: IncidentStatus,
        body: &str,
        declaration: Option<(&str, Impact)>,
    ) -> Result<ScheduledUpdate> {
        let current = self
            .scheduled(id)
            .await?
            .ok_or(StoreError::ScheduledNotFound(id))?;
        let mut update = ScheduledUpdateRecord::update_by_id(id)
            .publish_at(publish_at)
            .status(status.as_str())
            .body(body.trim())
            .error(None::<String>);
        if let (ScheduledKind::Declare { .. }, Some((title, impact))) = (&current.kind, declaration)
        {
            update = update
                .title(Some(title.trim().to_owned()))
                .impact(Some(impact.as_str().to_owned()));
        }
        update.exec(&mut self.db()).await?;
        self.scheduled(id)
            .await?
            .ok_or(StoreError::ScheduledNotFound(id))
    }

    /// Records why publishing failed; the item waits until it is edited.
    pub async fn fail_scheduled(&self, id: i64, error: &str) -> Result<()> {
        ScheduledUpdateRecord::update_by_id(id)
            .error(Some(error.to_owned()))
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    pub async fn cancel_scheduled(&self, id: i64) -> Result<bool> {
        let deleted =
            toasty::sql::statement(r#"DELETE FROM "scheduled_incident_updates" WHERE "id" = $1"#)
                .bind(id)
                .exec(&mut self.db())
                .await?;
        Ok(deleted > 0)
    }
}

/// Deletes the schedule `id` inside the publishing transaction; fails if
/// another publisher (or a cancel) got there first.
pub(crate) async fn take_scheduled(tx: &mut toasty::Transaction<'_>, id: i64) -> Result<()> {
    let deleted =
        toasty::sql::statement(r#"DELETE FROM "scheduled_incident_updates" WHERE "id" = $1"#)
            .bind(id)
            .exec(tx)
            .await?;
    if deleted == 0 {
        return Err(StoreError::ScheduledNotFound(id));
    }
    Ok(())
}

fn sorted(records: Vec<ScheduledUpdateRecord>) -> Result<Vec<ScheduledUpdate>> {
    let mut items = records
        .into_iter()
        .map(scheduled)
        .collect::<Result<Vec<_>>>()?;
    items.sort_by_key(|item| (item.publish_at, item.id));
    Ok(items)
}

fn scheduled(record: ScheduledUpdateRecord) -> Result<ScheduledUpdate> {
    const TABLE: &str = "scheduled_incident_updates";
    let kind = match record.incident_id {
        Some(incident_id) => ScheduledKind::Update { incident_id },
        None => {
            let keys: Vec<String> =
                serde_json::from_str(record.monitors.as_deref().unwrap_or("[]"))
                    .map_err(|e| StoreError::corrupt(TABLE, e))?;
            ScheduledKind::Declare {
                title: record.title.unwrap_or_default(),
                impact: convert::parse(TABLE, record.impact.as_deref().unwrap_or("minor"))?,
                monitors: keys
                    .iter()
                    .map(|key| convert::parse(TABLE, key))
                    .collect::<Result<_>>()?,
            }
        }
    };
    Ok(ScheduledUpdate {
        id: record.id,
        publish_at: record.publish_at,
        status: convert::parse(TABLE, &record.status)?,
        body: record.body,
        kind,
        actor: record.actor,
        error: record.error,
        created_at: record.created_at,
    })
}
