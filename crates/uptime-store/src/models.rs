//! Toasty models. These mirror the database schema and stay private to the
//! crate; the rest of the workspace talks to [`crate::Store`] in domain types.
//!
//! Enum-like columns (`state`, `health`, ...) are plain text holding the domain
//! type's `as_str()` form, which keeps raw SQL readable.

// Items are `pub` because Toasty's generated query builders must be nameable
// from the crate root; the module itself is private.
#![allow(unreachable_pub)]

use jiff::{Timestamp, civil::Date};
use toasty::Json;
use uptime_domain::{Alert, CheckSpec};

/// Monitor configuration (written by admins).
#[derive(Debug, toasty::Model)]
#[table = "monitors"]
pub struct MonitorRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[unique]
    pub key: String,
    pub name: String,
    #[column(type = text)]
    pub check: Json<CheckSpec>,
    pub interval_ms: i64,
    pub retry_interval_ms: i64,
    pub timeout_ms: i64,
    pub retries: i32,
    pub invert: bool,
    pub degraded_after_ms: Option<i64>,
    pub resend_every: i32,
    pub active: bool,
    /// Space-separated normalized tags; `NULL` when there are none.
    pub tags: Option<String>,
    /// Slash-separated group path (`prod/eu`); `NULL` when ungrouped.
    pub group_path: Option<String>,
    #[auto]
    pub created_at: Timestamp,
    #[auto]
    pub updated_at: Timestamp,
}

/// Scheduling and state for one monitor (written by the scheduler/workers).
#[derive(Debug, toasty::Model)]
#[table = "monitor_runtime"]
pub struct MonitorRuntimeRecord {
    #[key]
    pub monitor_id: i64,
    pub state: String,
    pub consecutive_failures: i32,
    #[index]
    pub next_run_at: Timestamp,
    pub scheduled_for: Option<Timestamp>,
    pub claimed_by: Option<String>,
    pub last_checked_at: Option<Timestamp>,
    pub last_latency_ms: Option<i64>,
    pub last_status_code: Option<i32>,
    pub last_error: Option<String>,
    pub state_changed_at: Option<Timestamp>,
    /// Push monitors: the latest heartbeat.
    pub last_push_at: Option<Timestamp>,
    pub last_push_up: Option<bool>,
    pub last_push_message: Option<String>,
    pub last_push_ms: Option<i64>,
    /// HTTPS monitors: when the certificate expires, and the warning
    /// threshold (days) already alerted about.
    pub cert_expires_at: Option<Timestamp>,
    pub cert_warned_days: Option<i32>,
}

/// One executed check (raw history, pruned after the retention window).
#[derive(Debug, toasty::Model)]
#[table = "check_results"]
pub struct CheckResultRecord {
    #[key]
    #[auto]
    pub id: i64,
    /// Indexed together with `checked_at` by migration 0001.
    pub monitor_id: i64,
    pub scheduled_for: Timestamp,
    #[index]
    pub checked_at: Timestamp,
    pub health: String,
    pub state_after: String,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i32>,
    pub error_kind: Option<String>,
    pub error: Option<String>,
    pub response_body: Option<String>,
    pub region: String,
}

/// Per-monitor, per-UTC-day counters (kept forever; feeds the 90-day bars).
#[derive(Debug, toasty::Model)]
#[table = "monitor_daily"]
#[key(monitor_id, day)]
pub struct MonitorDailyRecord {
    pub monitor_id: i64,
    pub day: Date,
    pub total: i64,
    pub up: i64,
    pub degraded: i64,
    pub pending: i64,
    pub down: i64,
    pub maintenance: i64,
    pub latency_sum_ms: i64,
    pub latency_count: i64,
    pub latency_max_ms: i64,
}

/// An admin who has signed in at least once. `github_id` pins the username.
#[derive(Debug, toasty::Model)]
#[table = "admin_users"]
pub struct AdminUserRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[unique]
    pub github_id: i64,
    /// Lowercase GitHub username.
    #[unique]
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    #[auto]
    pub created_at: Timestamp,
    pub last_login_at: Timestamp,
}

/// A signed-in browser session. Only the SHA-256 of the token is stored.
#[derive(Debug, toasty::Model)]
#[table = "sessions"]
pub struct SessionRecord {
    /// Hex-encoded SHA-256 of the session token.
    #[key]
    pub token_hash: String,
    #[index]
    pub user_id: i64,
    pub created_at: Timestamp,
    #[index]
    pub expires_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub user_agent: Option<String>,
}

/// A bearer token for the JSON API. Only the SHA-256 of the token is stored.
#[derive(Debug, toasty::Model)]
#[table = "api_tokens"]
pub struct ApiTokenRecord {
    #[key]
    #[auto]
    pub id: i64,
    pub name: String,
    /// Hex-encoded SHA-256 of the token.
    #[unique]
    pub token_hash: String,
    /// `read` or `write`.
    pub scope: String,
    /// The admin who created it (GitHub login).
    pub created_by: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

/// A public status page.
#[derive(Debug, toasty::Model)]
#[table = "status_pages"]
pub struct StatusPageRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[unique]
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub accent: Option<String>,
    pub theme: String,
    /// `NULL` for the default look (pixel).
    pub look: Option<String>,
    pub published: bool,
    /// "Back to the product" link.
    pub website_url: Option<String>,
    pub website_label: Option<String>,
    /// Uploaded images: file names in the media directory.
    pub logo: Option<String>,
    pub favicon: Option<String>,
    #[auto]
    pub created_at: Timestamp,
    #[auto]
    pub updated_at: Timestamp,
}

/// A titled group on a status page.
#[derive(Debug, toasty::Model)]
#[table = "page_sections"]
pub struct PageSectionRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[index]
    pub page_id: i64,
    pub name: String,
    pub position: i32,
}

/// A monitor shown in a section.
#[derive(Debug, toasty::Model)]
#[table = "page_components"]
pub struct PageComponentRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[index]
    pub section_id: i64,
    #[index]
    pub monitor_id: i64,
    pub label: Option<String>,
    pub position: i32,
}

/// A custom hostname serving a status page.
#[derive(Debug, toasty::Model)]
#[table = "custom_domains"]
pub struct CustomDomainRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[index]
    pub page_id: i64,
    #[unique]
    pub hostname: String,
    /// `pending`, `verified` or `failed`.
    pub status: String,
    pub last_error: Option<String>,
    pub verified_at: Option<Timestamp>,
    pub cert_ok_at: Option<Timestamp>,
    pub last_checked_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// Where alerts go (Slack, Discord, webhook).
#[derive(Debug, toasty::Model)]
#[table = "notification_channels"]
pub struct NotificationChannelRecord {
    #[key]
    #[auto]
    pub id: i64,
    pub name: String,
    /// `slack`, `discord` or `webhook`.
    pub kind: String,
    pub url: String,
    pub secret: Option<String>,
    pub default_on: bool,
    /// JSON [`uptime_domain::Routing`]; `NULL` for the default (send everything at once).
    pub routing: Option<String>,
    pub created_at: Timestamp,
}

/// Which channels a monitor alerts.
#[derive(Debug, toasty::Model)]
#[table = "monitor_channels"]
#[key(monitor_id, channel_id)]
pub struct MonitorChannelRecord {
    pub monitor_id: i64,
    #[index]
    pub channel_id: i64,
}

/// One alert to deliver to one channel (the outbox).
#[derive(Debug, toasty::Model)]
#[table = "notification_outbox"]
pub struct OutboxRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[index]
    pub channel_id: i64,
    pub monitor_id: Option<i64>,
    pub event: String,
    #[column(type = text)]
    pub payload: Json<Alert>,
    pub attempts: i32,
    #[index]
    pub next_attempt_at: Timestamp,
    pub created_at: Timestamp,
    pub sent_at: Option<Timestamp>,
    pub failed_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// A planned maintenance window.
#[derive(Debug, toasty::Model)]
#[table = "maintenances"]
pub struct MaintenanceRecord {
    #[key]
    #[auto]
    pub id: i64,
    pub title: String,
    pub description: Option<String>,
    #[index]
    pub starts_at: Timestamp,
    #[index]
    pub ends_at: Timestamp,
    /// `daily` or `weekly`; `NULL` for a one-off window.
    pub repeat: Option<String>,
    pub repeat_until: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// Monitors affected by a maintenance window.
#[derive(Debug, toasty::Model)]
#[table = "maintenance_monitors"]
#[key(maintenance_id, monitor_id)]
pub struct MaintenanceMonitorRecord {
    pub maintenance_id: i64,
    #[index]
    pub monitor_id: i64,
}

/// An incident: manual (declared by an admin) or automatic (DOWN → recovery).
#[derive(Debug, toasty::Model)]
#[table = "incidents"]
pub struct IncidentRecord {
    #[key]
    #[auto]
    pub id: i64,
    pub title: String,
    /// `none`, `minor`, `major` or `critical`.
    pub impact: String,
    /// `investigating`, `identified`, `monitoring` or `resolved`.
    pub status: String,
    /// `manual` or `auto`.
    pub kind: String,
    /// Automatic incidents: the monitor that went down.
    #[index]
    pub monitor_id: Option<i64>,
    #[index]
    pub started_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
}

/// One entry in an incident's timeline.
#[derive(Debug, toasty::Model)]
#[table = "incident_updates"]
pub struct IncidentUpdateRecord {
    #[key]
    #[auto]
    pub id: i64,
    #[index]
    pub incident_id: i64,
    pub status: String,
    pub body: String,
    /// Serialized check snapshot retained beyond raw check retention.
    pub check_json: Option<String>,
    pub created_at: Timestamp,
}

/// Monitors affected by an incident.
#[derive(Debug, toasty::Model)]
#[table = "incident_monitors"]
#[key(incident_id, monitor_id)]
pub struct IncidentMonitorRecord {
    pub incident_id: i64,
    #[index]
    pub monitor_id: i64,
}
