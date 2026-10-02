//! Notification channels, which monitors alert them, and the delivery outbox.
//!
//! Alerts are queued in the same transaction as the check result that caused
//! them ([`Store::record_check`]). PostgreSQL claims deliveries with
//! `FOR UPDATE SKIP LOCKED`; SQLite and Turso use an atomic update.

use std::{collections::HashMap, time::Duration};

use jiff::Timestamp;
use toasty::Json;
use uptime_domain::{Alert, AlertEvent, ChannelSpec, Routing};

use crate::{
    Backend, Result, Store, StoreError, convert,
    models::{MonitorChannelRecord, MonitorRecord, NotificationChannelRecord, OutboxRecord},
};

/// A stored channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub id: i64,
    pub spec: ChannelSpec,
    pub created_at: Timestamp,
}

/// A claimed delivery: send `alert` to `channel`, then report back with
/// [`Store::mark_delivered`] or [`Store::mark_delivery_failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub id: i64,
    pub channel: Channel,
    pub alert: Alert,
    /// Including this one.
    pub attempts: u32,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Not delivered yet; will be (re)tried.
    Pending,
    Sent,
    /// Gave up.
    Failed,
}

/// A delivery as shown in the admin console.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryLog {
    pub id: i64,
    pub channel_name: String,
    pub monitor_name: Option<String>,
    pub alert: Alert,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub created_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

const CLAIM_OUTBOX_SQL: &str = r#"
WITH due AS (
    SELECT "id" FROM "notification_outbox"
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL AND "next_attempt_at" <= $1
    ORDER BY "next_attempt_at", "id"
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
UPDATE "notification_outbox" o
SET "attempts" = o."attempts" + 1,
    "next_attempt_at" = $1 + $3 * INTERVAL '1 millisecond'
FROM due
WHERE o."id" = due."id"
RETURNING o."id"
"#;

const CLAIM_OUTBOX_SQLITE_SQL: &str = r#"
UPDATE "notification_outbox"
SET "attempts" = "attempts" + 1, "next_attempt_at" = ?3
WHERE "id" IN (
    SELECT "id" FROM "notification_outbox"
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL AND "next_attempt_at" <= ?1
    ORDER BY "next_attempt_at", "id" LIMIT ?2
)
RETURNING "id"
"#;

impl Store {
    pub async fn create_channel(&self, spec: &ChannelSpec, now: Timestamp) -> Result<Channel> {
        let record = toasty::create!(NotificationChannelRecord {
            name: spec.name.trim(),
            kind: spec.kind.as_str(),
            url: spec.url.as_str(),
            secret: spec.secret.clone(),
            default_on: spec.default_on,
            routing: routing_json(&spec.routing)?,
            created_at: now,
        })
        .exec(&mut self.db())
        .await?;
        channel(record)
    }

    pub async fn update_channel(&self, id: i64, spec: &ChannelSpec) -> Result<Channel> {
        if self.channel(id).await?.is_none() {
            return Err(StoreError::ChannelNotFound(id));
        }
        NotificationChannelRecord::update_by_id(id)
            .name(spec.name.trim())
            .kind(spec.kind.as_str())
            .url(spec.url.as_str())
            .secret(spec.secret.clone())
            .default_on(spec.default_on)
            .routing(routing_json(&spec.routing)?)
            .exec(&mut self.db())
            .await?;
        self.channel(id)
            .await?
            .ok_or(StoreError::ChannelNotFound(id))
    }

    /// Deletes a channel with its monitor links and queued deliveries.
    pub async fn delete_channel(&self, id: i64) -> Result<bool> {
        let deleted =
            toasty::sql::statement(r#"DELETE FROM "notification_channels" WHERE "id" = $1"#)
                .bind(id)
                .exec(&mut self.db())
                .await?;
        Ok(deleted > 0)
    }

    pub async fn channel(&self, id: i64) -> Result<Option<Channel>> {
        NotificationChannelRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
            .map(channel)
            .transpose()
    }

    /// Every channel, by name.
    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        let mut channels = NotificationChannelRecord::all()
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(channel)
            .collect::<Result<Vec<_>>>()?;
        channels.sort_by(|a, b| {
            a.spec
                .name
                .to_lowercase()
                .cmp(&b.spec.name.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        Ok(channels)
    }

    /// Channels pre-selected for new monitors.
    pub async fn default_channels(&self) -> Result<Vec<i64>> {
        Ok(self
            .list_channels()
            .await?
            .into_iter()
            .filter(|c| c.spec.default_on)
            .map(|c| c.id)
            .collect())
    }

    /// Replaces the channels `monitor` alerts.
    pub async fn set_monitor_channels(
        &self,
        monitor: uptime_domain::MonitorId,
        channels: &[i64],
    ) -> Result<()> {
        let mut wanted = channels.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if MonitorRecord::filter_by_id(monitor.0)
            .first()
            .exec(&mut tx)
            .await?
            .is_none()
        {
            return Err(StoreError::MonitorNotFound(monitor));
        }
        if !wanted.is_empty() {
            let found: Vec<i64> = NotificationChannelRecord::filter(
                NotificationChannelRecord::fields()
                    .id()
                    .in_list(wanted.clone()),
            )
            .exec(&mut tx)
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect();
            if let Some(missing) = wanted.iter().find(|id| !found.contains(id)) {
                return Err(StoreError::ChannelNotFound(*missing));
            }
        }
        toasty::sql::statement(r#"DELETE FROM "monitor_channels" WHERE "monitor_id" = $1"#)
            .bind(monitor.0)
            .exec(&mut tx)
            .await?;
        for channel_id in wanted {
            toasty::create!(MonitorChannelRecord {
                monitor_id: monitor.0,
                channel_id: channel_id,
            })
            .exec(&mut tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The channels `monitor` alerts, by id.
    pub async fn monitor_channels(&self, monitor: uptime_domain::MonitorId) -> Result<Vec<i64>> {
        let mut ids: Vec<i64> =
            MonitorChannelRecord::filter(MonitorChannelRecord::fields().monitor_id().eq(monitor.0))
                .exec(&mut self.db())
                .await?
                .into_iter()
                .map(|link| link.channel_id)
                .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// Claims up to `limit` due deliveries. Each is leased for `lease`: if it
    /// is neither delivered nor failed by then, it becomes due again.
    pub async fn claim_outbox(
        &self,
        now: Timestamp,
        limit: u32,
        lease: Duration,
    ) -> Result<Vec<Delivery>> {
        let mut db = self.db();
        let rows = if self.backend() == Backend::Postgres {
            toasty::sql::query(CLAIM_OUTBOX_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .bind(convert::millis(lease))
                .exec(&mut db)
                .await?
        } else {
            let lease_until = now
                .checked_add(lease)
                .map_err(|e| StoreError::corrupt("claim_outbox.lease", e))?;
            toasty::sql::query(CLAIM_OUTBOX_SQLITE_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .bind(self.raw_timestamp(lease_until))
                .exec(&mut db)
                .await?
        };
        let ids = rows
            .iter()
            .map(|row| convert::Row::new("claim_outbox", row)?.i64(0))
            .collect::<Result<Vec<_>>>()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records = OutboxRecord::filter(OutboxRecord::fields().id().in_list(ids))
            .exec(&mut db)
            .await?;
        let channel_ids: Vec<i64> = records.iter().map(|r| r.channel_id).collect();
        let channels: HashMap<i64, Channel> = NotificationChannelRecord::filter(
            NotificationChannelRecord::fields()
                .id()
                .in_list(channel_ids),
        )
        .exec(&mut db)
        .await?
        .into_iter()
        .map(|record| channel(record).map(|c| (c.id, c)))
        .collect::<Result<_>>()?;
        let mut deliveries = records
            .into_iter()
            .map(|record| {
                let channel = channels
                    .get(&record.channel_id)
                    .cloned()
                    .ok_or_else(|| StoreError::corrupt("claim_outbox", "channel vanished"))?;
                Ok(Delivery {
                    id: record.id,
                    channel,
                    alert: record.payload.0,
                    attempts: convert::count("notification_outbox.attempts", record.attempts)?,
                    created_at: record.created_at,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        deliveries.sort_by_key(|d| (d.created_at, d.id));
        Ok(deliveries)
    }

    pub async fn mark_delivered(&self, id: i64, now: Timestamp) -> Result<()> {
        OutboxRecord::update_by_id(id)
            .sent_at(Some(now))
            .last_error(None::<String>)
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// Records a failed attempt: retried at `retry_at`, or given up when `None`.
    pub async fn mark_delivery_failed(
        &self,
        id: i64,
        error: &str,
        retry_at: Option<Timestamp>,
    ) -> Result<()> {
        let update = OutboxRecord::update_by_id(id).last_error(Some(error.to_owned()));
        match retry_at {
            Some(at) => update.next_attempt_at(at).exec(&mut self.db()).await?,
            None => {
                update
                    .failed_at(Some(Timestamp::now()))
                    .exec(&mut self.db())
                    .await?
            }
        }
        Ok(())
    }

    /// Deletes finished (sent or given-up) deliveries queued before `cutoff`.
    /// Pending ones are kept however old. Returns how many were deleted.
    pub async fn prune_deliveries_before(&self, cutoff: Timestamp) -> Result<u64> {
        let deleted = toasty::sql::statement(
            r#"DELETE FROM "notification_outbox"
               WHERE "created_at" < $1 AND ("sent_at" IS NOT NULL OR "failed_at" IS NOT NULL)"#,
        )
        .bind(self.raw_timestamp(cutoff))
        .exec(&mut self.db())
        .await?;
        if deleted > 0 {
            tracing::info!(deleted, "pruned old alert deliveries");
        }
        Ok(deleted)
    }

    /// The most recent deliveries, newest first.
    pub async fn recent_deliveries(&self, limit: u32) -> Result<Vec<DeliveryLog>> {
        let mut db = self.db();
        let fields = OutboxRecord::fields();
        let records = OutboxRecord::all()
            .order_by(fields.id().desc())
            .limit(limit as usize)
            .exec(&mut db)
            .await?;
        let channels: HashMap<i64, String> = NotificationChannelRecord::all()
            .exec(&mut db)
            .await?
            .into_iter()
            .map(|c| (c.id, c.name))
            .collect();
        let monitor_ids: Vec<i64> = records.iter().filter_map(|r| r.monitor_id).collect();
        let monitors: HashMap<i64, String> = if monitor_ids.is_empty() {
            HashMap::new()
        } else {
            MonitorRecord::filter(MonitorRecord::fields().id().in_list(monitor_ids))
                .exec(&mut db)
                .await?
                .into_iter()
                .map(|m| (m.id, m.name))
                .collect()
        };
        records
            .into_iter()
            .map(|record| {
                let status = match (record.sent_at, record.failed_at) {
                    (Some(_), _) => DeliveryStatus::Sent,
                    (None, Some(_)) => DeliveryStatus::Failed,
                    (None, None) => DeliveryStatus::Pending,
                };
                Ok(DeliveryLog {
                    id: record.id,
                    channel_name: channels
                        .get(&record.channel_id)
                        .cloned()
                        .unwrap_or_default(),
                    monitor_name: record.monitor_id.and_then(|id| monitors.get(&id).cloned()),
                    alert: record.payload.0,
                    status,
                    attempts: convert::count("notification_outbox.attempts", record.attempts)?,
                    created_at: record.created_at,
                    finished_at: record.sent_at.or(record.failed_at),
                    last_error: record.last_error,
                })
            })
            .collect()
    }
}

fn routing_json(routing: &Routing) -> Result<Option<String>> {
    if routing.is_default() {
        return Ok(None);
    }
    serde_json::to_string(routing)
        .map(Some)
        .map_err(|e| StoreError::corrupt("notification_channels.routing", e))
}

/// Queues `alert` for each of `channels` inside an open transaction, as their
/// routing allows: muted events are dropped, quiet hours and escalation delay
/// the delivery.
///
/// A channel that escalates (waits before announcing an outage) only hears of
/// a recovery, or a reminder, if it was told about the outage.
pub(crate) async fn enqueue(
    tx: &mut toasty::Transaction<'_>,
    alert: &Alert,
    monitor_id: Option<i64>,
    channels: &[i64],
) -> Result<()> {
    let records = NotificationChannelRecord::filter(
        NotificationChannelRecord::fields()
            .id()
            .in_list(channels.to_vec()),
    )
    .exec(&mut *tx)
    .await?;
    for record in records {
        let channel_id = record.id;
        let routing = channel(record)?.spec.routing;
        let Some(due) = routing.send_at(alert.event, alert.at) else {
            continue;
        };
        if let (true, Some(monitor)) = (routing.escalates(), monitor_id) {
            let downs = OutboxRecord::filter(
                OutboxRecord::fields()
                    .channel_id()
                    .eq(channel_id)
                    .and(OutboxRecord::fields().monitor_id().eq(monitor))
                    .and(
                        OutboxRecord::fields()
                            .event()
                            .eq(AlertEvent::WentDown.as_str()),
                    ),
            )
            .exec(&mut *tx)
            .await?;
            // The latest down alert is this outage's.
            let latest = downs.iter().max_by_key(|d| (d.created_at, d.id));
            let told = latest.is_some_and(|d| d.sent_at.is_some());
            let waiting: Vec<i64> = latest
                .filter(|d| d.sent_at.is_none() && d.failed_at.is_none())
                .map(|d| d.id)
                .into_iter()
                .collect();
            match alert.event {
                AlertEvent::Recovered => {
                    for id in waiting {
                        delete_outbox(tx, id).await?;
                    }
                    if !told {
                        continue;
                    }
                }
                AlertEvent::Resend if !told => continue,
                _ => {}
            }
        }
        toasty::create!(OutboxRecord {
            channel_id: channel_id,
            monitor_id: monitor_id,
            event: alert.event.as_str(),
            payload: Json(alert.clone()),
            attempts: 0,
            next_attempt_at: due,
            created_at: alert.at,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn delete_outbox(tx: &mut toasty::Transaction<'_>, id: i64) -> Result<()> {
    toasty::sql::statement(r#"DELETE FROM "notification_outbox" WHERE "id" = $1"#)
        .bind(id)
        .exec(&mut *tx)
        .await?;
    Ok(())
}

fn channel(record: NotificationChannelRecord) -> Result<Channel> {
    Ok(Channel {
        id: record.id,
        spec: ChannelSpec {
            name: record.name,
            kind: convert::parse("notification_channels.kind", &record.kind)?,
            url: convert::parse("notification_channels.url", &record.url)?,
            secret: record.secret,
            default_on: record.default_on,
            routing: match record.routing.as_deref() {
                Some(json) => serde_json::from_str(json)
                    .map_err(|e| StoreError::corrupt("notification_channels.routing", e))?,
                None => Routing::default(),
            },
        },
        created_at: record.created_at,
    })
}
