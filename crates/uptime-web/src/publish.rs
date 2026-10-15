//! Publishing incidents: the one path every declaration and update takes,
//! whether it comes from the console, the JSON API or a schedule. It expands
//! template placeholders, writes the incident (the store queues subscriber
//! notifications in the same transaction, so a scheduled item notifies only
//! when it is published), refreshes the status pages showing it (here, in
//! open browsers and on other instances) and records the audit trail.
//!
//! [`publish_due`] publishes scheduled declarations and updates; the worker
//! runs it every [`SCHEDULE_EVERY`] ([`run_schedules`]).

use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use topcoat::Result;
use uptime_domain::{IncidentStatus, MonitorId, MonitorKey, Placeholders, expand};
use uptime_store::{Incident, NewIncident, ScheduledKind, ScheduledUpdate, StoreError};

use crate::{AppState, audit, fmt};

/// How often the worker looks for scheduled declarations and updates.
pub const SCHEDULE_EVERY: Duration = Duration::from_secs(15);

/// Where a publication comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// The console or the JSON API, now.
    Direct,
    /// A schedule coming due.
    Schedule(ScheduledUpdate),
}

/// Declares an incident as `actor`. The inner result is the store's answer
/// (e.g. unknown monitors); the outer one fails only on infrastructure errors.
pub(crate) async fn declare(
    state: &AppState,
    actor: &str,
    new: &NewIncident,
    via: Via,
    at: Timestamp,
) -> Result<std::result::Result<Incident, StoreError>> {
    let values = placeholders(state, &new.monitors, at).await?;
    let expanded = NewIncident {
        title: expand(&new.title, &values),
        message: expand(&new.message, &values),
        ..new.clone()
    };
    let result = match &via {
        Via::Direct => state.store.declare_incident(&expanded, at).await,
        Via::Schedule(scheduled) => {
            let scheduled = ScheduledUpdate {
                body: expanded.message.clone(),
                kind: ScheduledKind::Declare {
                    title: expanded.title.clone(),
                    impact: expanded.impact,
                    monitors: expanded.monitors.clone(),
                },
                ..scheduled.clone()
            };
            state.store.declare_scheduled_incident(&scheduled, at).await
        }
    };
    let incident = match result {
        Ok(incident) => incident,
        Err(error) => return Ok(Err(error)),
    };
    refresh(state, &incident.monitor_ids).await?;
    tracing::info!(incident = incident.id, by = actor, "incident declared");
    audit::record(
        state,
        actor,
        "incident.declare",
        audit::target("incident", incident.id),
        format!("Declared incident {}{}", incident.title, suffix(&via)),
        with_schedule(
            json!({
                "title": incident.title,
                "impact": expanded.impact.as_str(),
                "status": expanded.status.as_str(),
                "message": expanded.message,
                "monitors": expanded.monitors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
            &via,
        ),
    )
    .await?;
    Ok(Ok(incident))
}

/// Posts an update to incident `id` as `actor`; see [`declare`] for the results.
pub(crate) async fn update(
    state: &AppState,
    actor: &str,
    id: i64,
    status: IncidentStatus,
    message: &str,
    via: Via,
    at: Timestamp,
) -> Result<std::result::Result<Incident, StoreError>> {
    let Some(current) = state.store.incident(id).await? else {
        return Ok(Err(StoreError::IncidentNotFound(id)));
    };
    let keys = monitor_keys(state, &current.monitor_ids).await?;
    let values = placeholders(state, &keys, current.started_at).await?;
    let message = expand(message, &values);
    let result = match &via {
        Via::Direct => {
            state
                .store
                .post_incident_update(id, status, &message, at)
                .await
        }
        Via::Schedule(scheduled) => {
            let scheduled = ScheduledUpdate {
                status,
                body: message.clone(),
                ..scheduled.clone()
            };
            state.store.post_scheduled_update(&scheduled, at).await
        }
    };
    let incident = match result {
        Ok(incident) => incident,
        Err(error) => return Ok(Err(error)),
    };
    refresh(state, &incident.monitor_ids).await?;
    tracing::info!(incident = id, status = %status, by = actor, "incident updated");
    audit::record(
        state,
        actor,
        "incident.update",
        audit::target("incident", id),
        format!(
            "Posted a {} update to {}{}",
            status.label().to_lowercase(),
            incident.title,
            suffix(&via)
        ),
        with_schedule(
            json!({ "status": status.as_str(), "message": message }),
            &via,
        ),
    )
    .await?;
    Ok(Ok(incident))
}

/// Publishes every scheduled item due at `now`. Returns how many were published.
pub async fn publish_due(state: &AppState, now: Timestamp) -> Result<usize> {
    let mut published = 0;
    for scheduled in state.store.due_scheduled(now, 50).await? {
        let outcome = match &scheduled.kind {
            ScheduledKind::Declare {
                title,
                impact,
                monitors,
            } => {
                let new = NewIncident {
                    title: title.clone(),
                    impact: *impact,
                    status: scheduled.status,
                    message: scheduled.body.clone(),
                    monitors: monitors.clone(),
                };
                declare(
                    state,
                    &scheduled.actor,
                    &new,
                    Via::Schedule(scheduled.clone()),
                    now,
                )
                .await?
                .map(|_| ())
            }
            ScheduledKind::Update { incident_id } => update(
                state,
                &scheduled.actor,
                *incident_id,
                scheduled.status,
                &scheduled.body,
                Via::Schedule(scheduled.clone()),
                now,
            )
            .await?
            .map(|_| ()),
        };
        match outcome {
            Ok(()) => published += 1,
            // Published elsewhere, or cancelled meanwhile.
            Err(StoreError::ScheduledNotFound(_)) => {}
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(scheduled = scheduled.id, error = %message, "publishing a scheduled incident update failed");
                state.store.fail_scheduled(scheduled.id, &message).await?;
                audit::record(
                    state,
                    "system",
                    "incident.schedule_failed",
                    scheduled
                        .incident_id()
                        .and_then(|id| audit::target("incident", id)),
                    format!("A scheduled incident update could not be published: {message}"),
                    json!({ "scheduled": scheduled.id, "error": message }),
                )
                .await?;
            }
        }
    }
    Ok(published)
}

/// Publishes due schedules every [`SCHEDULE_EVERY`] until `shutdown`.
pub async fn run_schedules(state: AppState, shutdown: CancellationToken) {
    loop {
        if let Err(error) = publish_due(&state, Timestamp::now()).await {
            tracing::warn!(error = %error, "publishing scheduled incident updates failed");
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(SCHEDULE_EVERY) => {}
        }
    }
}

fn suffix(via: &Via) -> &'static str {
    match via {
        Via::Direct => "",
        Via::Schedule(_) => " (scheduled)",
    }
}

fn with_schedule(mut details: Value, via: &Via) -> Value {
    if let Via::Schedule(scheduled) = via {
        details["scheduled"] = json!({
            "id": scheduled.id,
            "publish_at": scheduled.publish_at,
            "scheduled_at": scheduled.created_at,
        });
    }
    details
}

/// The status pages showing any of `monitors` rebuild, and open copies refresh.
async fn refresh(state: &AppState, monitors: &[MonitorId]) -> Result<()> {
    state.pages.clear();
    for (slug, _) in pages_showing(state, monitors).await? {
        state.page_changed(&slug);
    }
    Ok(())
}

/// Slug and title of every page with a component for one of `monitors`.
async fn pages_showing(state: &AppState, monitors: &[MonitorId]) -> Result<Vec<(String, String)>> {
    let keys = monitor_keys(state, monitors).await?;
    let mut pages = Vec::new();
    for summary in state.store.list_pages().await? {
        let Some(page) = state.store.page(summary.id).await? else {
            continue;
        };
        if page.spec.monitor_keys().any(|key| keys.contains(key)) {
            pages.push((page.spec.slug.to_string(), page.spec.title.clone()));
        }
    }
    Ok(pages)
}

async fn monitor_keys(state: &AppState, monitors: &[MonitorId]) -> Result<Vec<MonitorKey>> {
    Ok(state
        .store
        .list_monitors()
        .await?
        .into_iter()
        .filter(|m| monitors.contains(&m.id))
        .map(|m| m.spec.key)
        .collect())
}

/// What `{monitor}`, `{monitors}`, `{page}` and `{started_at}` stand for.
async fn placeholders(
    state: &AppState,
    monitors: &[MonitorKey],
    started_at: Timestamp,
) -> Result<Placeholders> {
    let all = state.store.list_monitors().await?;
    let names: Vec<String> = monitors
        .iter()
        .filter_map(|key| all.iter().find(|m| &m.spec.key == key))
        .map(|m| m.spec.name.clone())
        .collect();
    let ids: Vec<MonitorId> = all
        .iter()
        .filter(|m| monitors.contains(&m.spec.key))
        .map(|m| m.id)
        .collect();
    let pages: Vec<String> = pages_showing(state, &ids)
        .await?
        .into_iter()
        .map(|(_, title)| title)
        .collect();
    Ok(Placeholders {
        monitor: names.first().cloned().unwrap_or_default(),
        monitors: names.join(", "),
        page: pages.join(", "),
        started_at: fmt::when(started_at),
    })
}
