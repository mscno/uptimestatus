//! Status page subscriptions: visitors who asked to hear about incidents,
//! maintenance and component changes, by email or webhook.
//!
//! A [`PageEvent`] is what subscribers are told. It carries only what the
//! public page shows (never targets, errors or response bodies), plus the
//! monitors involved, so each page can name its own components and each
//! subscriber's component selection can be honored.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{Impact, IncidentStatus, MonitorKey};

/// How a subscriber is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriberKind {
    Email,
    Webhook,
}

/// Where a subscription stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriberStatus {
    /// Waiting for the confirmation link; nothing else is sent.
    Pending,
    /// Confirmed: receives notifications.
    Active,
    /// Left through an unsubscribe link (or removed by an admin).
    Unsubscribed,
    /// The address bounced or complained; never sent to again.
    Suppressed,
    /// A webhook that kept failing.
    Disabled,
}

macro_rules! text_enum {
    ($ty:ty { $($variant:ident => $text:literal, $label:literal),+ $(,)? }) => {
        impl $ty {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
        }

        impl FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!("unknown value `{other}`")),
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(SubscriberKind {
    Email => "email", "Email",
    Webhook => "webhook", "Webhook",
});

text_enum!(SubscriberStatus {
    Pending => "pending", "Awaiting confirmation",
    Active => "active", "Active",
    Unsubscribed => "unsubscribed", "Unsubscribed",
    Suppressed => "suppressed", "Suppressed",
    Disabled => "disabled", "Disabled",
});

/// An incident as subscribers see it: the public timeline's latest entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentNotice {
    pub id: i64,
    pub title: String,
    pub status: IncidentStatus,
    pub impact: Impact,
    /// The update being announced.
    pub message: String,
    pub started_at: Timestamp,
    #[serde(default)]
    pub resolved_at: Option<Timestamp>,
    /// When this update was posted.
    pub at: Timestamp,
    /// Affected monitors (named per page when rendered).
    pub monitors: Vec<MonitorKey>,
}

/// A maintenance window (one occurrence, for recurring ones).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceNotice {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub monitors: Vec<MonitorKey>,
}

/// A component's status set by an admin (manual components and overrides).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentNotice {
    /// The component's key in subscriber selections ([`Selection`]).
    pub key: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    pub at: Timestamp,
    /// The monitor behind the component, if any.
    #[serde(default)]
    pub monitors: Vec<MonitorKey>,
}

/// What subscribers are told. The serialized `type` is the webhook event
/// name, e.g. `incident.created`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum PageEvent {
    #[serde(rename = "incident.created")]
    IncidentCreated(IncidentNotice),
    #[serde(rename = "incident.updated")]
    IncidentUpdated(IncidentNotice),
    #[serde(rename = "incident.resolved")]
    IncidentResolved(IncidentNotice),
    #[serde(rename = "maintenance.scheduled")]
    MaintenanceScheduled(MaintenanceNotice),
    #[serde(rename = "maintenance.started")]
    MaintenanceStarted(MaintenanceNotice),
    #[serde(rename = "maintenance.completed")]
    MaintenanceCompleted(MaintenanceNotice),
    #[serde(rename = "component.status_changed")]
    ComponentStatusChanged(ComponentNotice),
}

impl PageEvent {
    /// The webhook event name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::IncidentCreated(_) => "incident.created",
            Self::IncidentUpdated(_) => "incident.updated",
            Self::IncidentResolved(_) => "incident.resolved",
            Self::MaintenanceScheduled(_) => "maintenance.scheduled",
            Self::MaintenanceStarted(_) => "maintenance.started",
            Self::MaintenanceCompleted(_) => "maintenance.completed",
            Self::ComponentStatusChanged(_) => "component.status_changed",
        }
    }

    /// An incident announcement for `status` (created, resolved, or an update).
    pub fn incident(created: bool, notice: IncidentNotice) -> Self {
        if notice.status == IncidentStatus::Resolved {
            Self::IncidentResolved(notice)
        } else if created {
            Self::IncidentCreated(notice)
        } else {
            Self::IncidentUpdated(notice)
        }
    }

    /// The monitors the event is about.
    pub fn monitors(&self) -> &[MonitorKey] {
        match self {
            Self::IncidentCreated(n) | Self::IncidentUpdated(n) | Self::IncidentResolved(n) => {
                &n.monitors
            }
            Self::MaintenanceScheduled(n)
            | Self::MaintenanceStarted(n)
            | Self::MaintenanceCompleted(n) => &n.monitors,
            Self::ComponentStatusChanged(n) => &n.monitors,
        }
    }

    /// When it happened.
    pub fn at(&self) -> Timestamp {
        match self {
            Self::IncidentCreated(n) | Self::IncidentUpdated(n) | Self::IncidentResolved(n) => n.at,
            Self::MaintenanceScheduled(n) | Self::MaintenanceStarted(n) => n.starts_at,
            Self::MaintenanceCompleted(n) => n.ends_at,
            Self::ComponentStatusChanged(n) => n.at,
        }
    }

    /// One line for subject lines, e.g. "Investigating: API errors".
    pub fn headline(&self) -> String {
        match self {
            Self::IncidentCreated(n) | Self::IncidentUpdated(n) => {
                format!("{}: {}", n.status.label(), n.title)
            }
            Self::IncidentResolved(n) => format!("Resolved: {}", n.title),
            Self::MaintenanceScheduled(n) => format!("Scheduled maintenance: {}", n.title),
            Self::MaintenanceStarted(n) => format!("Maintenance in progress: {}", n.title),
            Self::MaintenanceCompleted(n) => format!("Maintenance completed: {}", n.title),
            Self::ComponentStatusChanged(n) => {
                format!("{} is now {}", n.name, n.status.replace('_', " "))
            }
        }
    }

    /// A component key the event touches, besides its monitors (manual components).
    fn component_key(&self) -> Option<&str> {
        match self {
            Self::ComponentStatusChanged(n) => Some(&n.key),
            _ => None,
        }
    }
}

/// The components a subscriber follows: their keys (a monitor component's
/// key is its monitor key; manual components use their own key). Empty means
/// everything on the page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Selection(pub Vec<String>);

impl Selection {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.is_all() || self.0.iter().any(|k| k == key)
    }

    /// Whether a subscriber with this selection hears about `event`. Events
    /// about no particular component (no monitors, no component key) reach
    /// everyone.
    pub fn wants(&self, event: &PageEvent) -> bool {
        if self.is_all() {
            return true;
        }
        let monitors = event.monitors();
        if monitors.is_empty() && event.component_key().is_none() {
            return true;
        }
        monitors.iter().any(|m| self.contains(m.as_str()))
            || event.component_key().is_some_and(|k| self.contains(k))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + s).unwrap()
    }

    fn notice(status: IncidentStatus, monitors: &[&str]) -> IncidentNotice {
        IncidentNotice {
            id: 7,
            title: "API errors".into(),
            status,
            impact: Impact::Major,
            message: "We are looking into it.".into(),
            started_at: t(0),
            resolved_at: None,
            at: t(10),
            monitors: monitors.iter().map(|m| m.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn incident_events_follow_their_status() {
        let created = PageEvent::incident(true, notice(IncidentStatus::Investigating, &["api"]));
        assert_eq!(created.type_name(), "incident.created");
        assert_eq!(created.headline(), "Investigating: API errors");
        let updated = PageEvent::incident(false, notice(IncidentStatus::Identified, &["api"]));
        assert_eq!(updated.type_name(), "incident.updated");
        let resolved = PageEvent::incident(false, notice(IncidentStatus::Resolved, &["api"]));
        assert_eq!(resolved.type_name(), "incident.resolved");
        assert_eq!(resolved.headline(), "Resolved: API errors");
    }

    #[test]
    fn events_serialize_with_their_webhook_type() {
        let event = PageEvent::incident(true, notice(IncidentStatus::Investigating, &["api"]));
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "incident.created");
        assert_eq!(json["data"]["title"], "API errors");
        assert_eq!(serde_json::from_value::<PageEvent>(json).unwrap(), event);
    }

    #[test]
    fn selections_filter_by_component() {
        let api = PageEvent::incident(true, notice(IncidentStatus::Investigating, &["api"]));
        let db = PageEvent::incident(true, notice(IncidentStatus::Investigating, &["db"]));
        let general = PageEvent::incident(true, notice(IncidentStatus::Investigating, &[]));
        let only_api = Selection(vec!["api".into()]);
        assert!(Selection::all().wants(&db));
        assert!(only_api.wants(&api));
        assert!(!only_api.wants(&db));
        assert!(only_api.wants(&general), "page-wide news reaches everyone");
        let manual = PageEvent::ComponentStatusChanged(ComponentNotice {
            key: "manual:3".into(),
            name: "Billing".into(),
            status: "degraded".into(),
            previous: Some("operational".into()),
            note: None,
            at: t(0),
            monitors: vec![],
        });
        assert!(!only_api.wants(&manual));
        assert!(Selection(vec!["manual:3".into()]).wants(&manual));
        assert_eq!(manual.headline(), "Billing is now degraded");
    }

    #[test]
    fn kinds_and_statuses_round_trip() {
        for kind in SubscriberKind::ALL {
            assert_eq!(kind.as_str().parse::<SubscriberKind>(), Ok(*kind));
        }
        for status in SubscriberStatus::ALL {
            assert_eq!(status.as_str().parse::<SubscriberStatus>(), Ok(*status));
        }
    }
}
