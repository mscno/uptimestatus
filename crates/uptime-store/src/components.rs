//! Statuses admins set on status page components: manual components' status,
//! and overrides of monitor components. Every change is kept as an event; the
//! events are the components' uptime history ([`uptime_domain::component`]).

use std::collections::{BTreeMap, HashMap};

use jiff::{Timestamp, civil::Date};
use uptime_domain::{ComponentStatus, MonitorId, StatusChange, Tally, component::component_days};

use crate::{
    Result, Store, StoreError, convert,
    models::{ComponentStatusEventRecord, MonitorRecord, StatusPageRecord},
};

/// One recorded change of a component's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentStatusEvent {
    pub id: i64,
    pub page_id: i64,
    /// The component's key on the page.
    pub key: String,
    pub at: Timestamp,
    /// `None`: a monitor component's override was cleared.
    pub status: Option<ComponentStatus>,
    pub previous: Option<ComponentStatus>,
    pub note: Option<String>,
    pub actor: String,
}

impl Store {
    /// Sets a manual component's status, or overrides a monitor component's
    /// (`None` clears the override). Returns the recorded change.
    #[tracing::instrument(skip(self, note))]
    pub async fn set_component_status(
        &self,
        page_id: i64,
        key: &str,
        status: Option<ComponentStatus>,
        note: Option<&str>,
        actor: &str,
        at: Timestamp,
    ) -> Result<ComponentStatusEvent> {
        let page = self
            .page(page_id)
            .await?
            .ok_or(StoreError::PageNotFound(page_id))?;
        let component = page
            .spec
            .component(key)
            .ok_or_else(|| StoreError::ComponentNotFound(key.to_owned()))?;
        if component.is_manual() && status.is_none() {
            return Err(StoreError::StatusRequired(key.to_owned()));
        }
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let previous = events(&mut tx, page_id)
            .await?
            .into_iter()
            .rfind(|event| event.key == key)
            .and_then(|event| event.status);
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        let record = toasty::create!(ComponentStatusEventRecord {
            page_id: page_id,
            component_key: key,
            at: at,
            status: status.map(|s| s.as_str().to_owned()),
            previous: previous.map(|s| s.as_str().to_owned()),
            note: note.map(str::to_owned),
            actor: actor,
        })
        .exec(&mut tx)
        .await?;
        // Subscribers hear about statuses set by admins; clearing an override
        // hands the component back to its checks, which is not news.
        if let Some(status) = status {
            let name = match (component.label(), component.monitor_key()) {
                (Some(label), _) => label.to_owned(),
                (None, Some(monitor)) => MonitorRecord::filter_by_key(monitor.as_str())
                    .first()
                    .exec(&mut tx)
                    .await?
                    .map_or_else(|| key.to_owned(), |m| m.name),
                (None, None) => key.to_owned(),
            };
            let notice = uptime_domain::ComponentNotice {
                key: key.to_owned(),
                name,
                status: status.as_str().to_owned(),
                previous: previous.map(|s| s.as_str().to_owned()),
                note: note.map(str::to_owned),
                at,
                monitors: component.monitor_key().cloned().into_iter().collect(),
            };
            crate::subscribers::fan_out(
                &mut tx,
                self.subscriptions_enabled(),
                &uptime_domain::PageEvent::ComponentStatusChanged(notice),
                &format!("component:{}", record.id),
                Some(page_id),
                false,
                at,
            )
            .await?;
        }
        tx.commit().await?;
        event(record)
    }

    /// Each component's status changes, oldest first, by component key.
    pub async fn component_changes(
        &self,
        page_id: i64,
    ) -> Result<HashMap<String, Vec<StatusChange>>> {
        let mut changes: HashMap<String, Vec<StatusChange>> = HashMap::new();
        for event in events(&mut self.db(), page_id).await? {
            changes.entry(event.key).or_default().push(StatusChange {
                at: event.at,
                status: event.status,
            });
        }
        Ok(changes)
    }

    /// The page's most recent status changes, newest first.
    pub async fn component_events(
        &self,
        page_id: i64,
        limit: usize,
    ) -> Result<Vec<ComponentStatusEvent>> {
        let mut events = events(&mut self.db(), page_id).await?;
        events.reverse();
        events.truncate(limit);
        Ok(events)
    }

    /// A component's per-UTC-day tallies for `from..=to`, as status pages and
    /// reports show them: a manual component's from its status history (in
    /// seconds), a monitor component's from its checks, with override time
    /// blended in (then in seconds too). Today counts up to `now`.
    pub async fn component_daily_tallies(
        &self,
        page_id: i64,
        key: &str,
        from: Date,
        to: Date,
        now: Timestamp,
    ) -> Result<BTreeMap<Date, Tally>> {
        let page = self
            .page(page_id)
            .await?
            .ok_or(StoreError::PageNotFound(page_id))?;
        let component = page
            .spec
            .component(key)
            .ok_or_else(|| StoreError::ComponentNotFound(key.to_owned()))?;
        let changes = self
            .component_changes(page_id)
            .await?
            .remove(key)
            .unwrap_or_default();
        let monitor = match component.monitor_key() {
            Some(monitor_key) => {
                let id = MonitorRecord::filter_by_key(monitor_key.as_str())
                    .first()
                    .exec(&mut self.db())
                    .await?
                    .map(|monitor| MonitorId(monitor.id))
                    .ok_or_else(|| StoreError::UnknownMonitors(vec![monitor_key.clone()]))?;
                Some(
                    self.daily_tallies(&[id], from, to)
                        .await?
                        .remove(&id)
                        .unwrap_or_default(),
                )
            }
            None => None,
        };
        Ok(component_days(monitor.as_ref(), &changes, from, to, now))
    }
}

/// Every event of a page, oldest first.
async fn events(
    executor: &mut dyn toasty::Executor,
    page_id: i64,
) -> Result<Vec<ComponentStatusEvent>> {
    if StatusPageRecord::filter_by_id(page_id)
        .first()
        .exec(executor)
        .await?
        .is_none()
    {
        return Err(StoreError::PageNotFound(page_id));
    }
    let mut events = ComponentStatusEventRecord::filter_by_page_id(page_id)
        .exec(executor)
        .await?
        .into_iter()
        .map(event)
        .collect::<Result<Vec<_>>>()?;
    events.sort_by_key(|event| (event.at, event.id));
    Ok(events)
}

fn event(record: ComponentStatusEventRecord) -> Result<ComponentStatusEvent> {
    let status = |value: Option<String>, column| {
        value
            .as_deref()
            .map(|s| convert::parse::<ComponentStatus>(column, s))
            .transpose()
    };
    Ok(ComponentStatusEvent {
        id: record.id,
        page_id: record.page_id,
        key: record.component_key,
        at: record.at,
        status: status(record.status, "component_status_events.status")?,
        previous: status(record.previous, "component_status_events.previous")?,
        note: record.note,
        actor: record.actor,
    })
}
