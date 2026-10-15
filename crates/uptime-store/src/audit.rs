//! The audit trail: who changed what, and when.
//!
//! Every admin console mutation and write API call records one event after
//! it succeeds (see `uptime_web::audit`). Details are JSON and must never hold
//! secrets: callers pass redacted values. Events are kept forever.

use jiff::Timestamp;
use serde_json::Value;

use crate::{
    Backend, Result, Store, StoreError,
    convert::{Row, like_escape},
    models::AuditEventRecord,
};

/// What an event is about, e.g. `monitor` `12`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditTarget {
    pub kind: String,
    pub id: String,
}

impl AuditTarget {
    pub fn new(kind: &str, id: impl ToString) -> Self {
        Self {
            kind: kind.to_owned(),
            id: id.to_string(),
        }
    }
}

/// An event to record.
#[derive(Clone, Debug, PartialEq)]
pub struct NewAuditEvent {
    /// An admin login, `api:<token name>` or `system`.
    pub actor: String,
    /// Dotted, e.g. `monitor.update`.
    pub action: String,
    pub target: Option<AuditTarget>,
    /// One line for people.
    pub summary: String,
    /// Changed fields and context; never secrets.
    pub details: Value,
}

/// A recorded event.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEvent {
    pub id: i64,
    pub at: Timestamp,
    pub actor: String,
    pub action: String,
    pub target: Option<AuditTarget>,
    pub summary: String,
    pub details: Value,
}

/// Which events to list: newest first, `limit` of them, older than `before`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Exact actor.
    pub actor: Option<String>,
    /// An action or its prefix: `monitor` matches `monitor.update`.
    pub action: Option<String>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    /// Only events with an id below this (the next page).
    pub before: Option<i64>,
    pub limit: u32,
}

impl Store {
    /// Records an event at `at`. Returns its id.
    pub async fn record_audit(&self, event: &NewAuditEvent, at: Timestamp) -> Result<i64> {
        let details = (!event.details.is_null())
            .then(|| serde_json::to_string(&event.details))
            .transpose()
            .map_err(|e| StoreError::corrupt("audit_events.details", e))?;
        let record = toasty::create!(AuditEventRecord {
            at: at,
            actor: event.actor.as_str(),
            action: event.action.as_str(),
            target_kind: event.target.as_ref().map(|t| t.kind.clone()),
            target_id: event.target.as_ref().map(|t| t.id.clone()),
            summary: event.summary.as_str(),
            details: details,
        })
        .exec(&mut self.db())
        .await?;
        Ok(record.id)
    }

    /// Events matching `query`, newest first.
    pub async fn audit_events(&self, query: &AuditQuery) -> Result<Vec<AuditEvent>> {
        let postgres = self.backend() == Backend::Postgres;
        let mut sql = String::from(r#"SELECT "id" FROM "audit_events" WHERE 1 = 1"#);
        let mut binds: Vec<toasty::stmt::Value> = Vec::new();
        let mut bind = |sql: &mut String, clause: &str, value: toasty::stmt::Value| {
            binds.push(value);
            let n = binds.len();
            sql.push_str(&clause.replace(
                '#',
                &if postgres {
                    format!("${n}")
                } else {
                    format!("?{n}")
                },
            ));
        };
        if let Some(actor) = &query.actor {
            bind(&mut sql, r#" AND "actor" = #"#, actor.clone().into());
        }
        if let Some(action) = &query.action {
            bind(&mut sql, r#" AND ("action" = #"#, action.clone().into());
            bind(
                &mut sql,
                r#" OR "action" LIKE # ESCAPE '\')"#,
                format!("{}.%", like_escape(action)).into(),
            );
        }
        if let Some(kind) = &query.target_kind {
            bind(&mut sql, r#" AND "target_kind" = #"#, kind.clone().into());
        }
        if let Some(id) = &query.target_id {
            bind(&mut sql, r#" AND "target_id" = #"#, id.clone().into());
        }
        if let Some(before) = query.before {
            bind(&mut sql, r#" AND "id" < #"#, before.into());
        }
        bind(
            &mut sql,
            r#" ORDER BY "id" DESC LIMIT #"#,
            i64::from(query.limit.max(1)).into(),
        );
        let mut statement = toasty::sql::query(sql);
        for value in binds {
            statement = statement.bind(value);
        }
        let ids = statement
            .exec(&mut self.db())
            .await?
            .iter()
            .map(|row| Row::new("audit_events", row)?.i64(0))
            .collect::<Result<Vec<_>>>()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut events = AuditEventRecord::filter(AuditEventRecord::fields().id().in_list(ids))
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(event)
            .collect::<Result<Vec<_>>>()?;
        events.sort_by_key(|e| std::cmp::Reverse(e.id));
        Ok(events)
    }

    /// Every actor that has recorded an event, sorted (for filters).
    pub async fn audit_actors(&self) -> Result<Vec<String>> {
        let rows = toasty::sql::query(r#"SELECT DISTINCT "actor" FROM "audit_events""#)
            .exec(&mut self.db())
            .await?;
        let mut actors = rows
            .iter()
            .map(|row| Ok(Row::new("audit_events.actor", row)?.string(0)?.to_owned()))
            .collect::<Result<Vec<_>>>()?;
        actors.sort();
        Ok(actors)
    }
}

fn event(record: AuditEventRecord) -> Result<AuditEvent> {
    Ok(AuditEvent {
        id: record.id,
        at: record.at,
        actor: record.actor,
        action: record.action,
        target: record
            .target_kind
            .zip(record.target_id)
            .map(|(kind, id)| AuditTarget { kind, id }),
        summary: record.summary,
        details: match record.details.as_deref() {
            Some(json) => serde_json::from_str(json)
                .map_err(|e| StoreError::corrupt("audit_events.details", e))?,
            None => Value::Null,
        },
    })
}
