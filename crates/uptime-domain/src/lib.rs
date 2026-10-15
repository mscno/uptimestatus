//! Pure domain model for uptimestatus.
//!
//! Everything in this crate is deterministic and free of I/O. Probes produce an
//! [`Observation`], [`evaluate`] turns it into a [`Verdict`] under a monitor's
//! [`CheckPolicy`], and [`state::apply`] advances the monitor's [`Runtime`].

pub mod api_token;
pub mod auth;
pub mod cert;
pub mod check;
pub mod component;
pub mod config;
pub mod domain;
pub mod email;
pub mod group;
pub mod incident;
pub mod json_rule;
pub mod latency;
pub mod maintenance;
pub mod monitor;
pub mod notify;
pub mod observation;
pub mod page;
pub mod policy;
pub mod protocol;
pub mod push;
pub mod report;
pub mod routing;
pub mod sla;
pub mod state;
pub mod status_codes;
pub mod subscription;
pub mod template;
pub mod uptime;
pub mod verdict;

pub use api_token::{TOKEN_PREFIX, TokenScope, bearer_token};
pub use auth::{Allowlist, LoginDenied, authorize};
pub use check::{
    CheckSpec, DnsCheck, DnsRecordType, HttpAuth, HttpCheck, HttpMethod, KeywordRule, PushCheck,
    TcpCheck,
};
pub use component::{ComponentStatus, StatusChange, UnknownComponentStatus};
pub use config::ConfigFile;
pub use domain::{DnsAnswers, Edge, Hostname, HostnameError, points_at_edge};
pub use email::{DnsRecord, DomainState, EmailAddress, EmailError, Mailbox, SenderDomain};
pub use group::{GroupCheck, GroupError, GroupMode, GroupOutcome, Member, normalize_group, rollup};
pub use incident::{Impact, IncidentKind, IncidentStatus};
pub use json_rule::JsonRule;
pub use maintenance::{MaintenanceError, MaintenanceSpec, Repeat};
pub use monitor::{
    MAX_TAG_LEN, MAX_TAGS, MonitorId, MonitorKey, MonitorKeyError, MonitorSpec, TagError,
    normalize_tags,
};
pub use notify::{
    Alert, AlertEvent, AlertMonitor, ChannelError, ChannelKind, ChannelSpec, MAX_EMAIL_RECIPIENTS,
    format_duration,
};
pub use observation::{FailureKind, Observation, UnknownFailureKind};
pub use page::{
    Accent, BarLevel, ComponentKey, ComponentSpec, LayoutError, Look, MAX_CUSTOM_CSS, PageError,
    PageSlug, PageSpec, PageStatus, SectionSpec, Theme, Website, bar_level, format_layout,
    page_status, parse_layout, validate_custom_css,
};
pub use policy::{CheckPolicy, PolicyError};
pub use protocol::{CheckError, DatabaseCheck, DatabaseEngine, GrpcCheck, MqttCheck, PingCheck};
pub use push::Push;
pub use report::Report;
pub use routing::{QuietHours, Routing, RoutingError};
pub use sla::{Month, MonthFigures, SlaTarget, month_figures, overall_uptime};
pub use state::{MonitorState, Runtime, Step, Transition, UnknownState};
pub use status_codes::{StatusRanges, StatusRangesError};
pub use subscription::{
    ComponentNotice, IncidentNotice, MaintenanceNotice, PageEvent, Selection, SubscriberKind,
    SubscriberStatus,
};
pub use template::{IncidentTemplate, Placeholders, TemplateError, expand};
pub use uptime::{Tally, UptimeWindows};
pub use verdict::{DownReason, Health, UnknownHealth, Verdict, evaluate};
