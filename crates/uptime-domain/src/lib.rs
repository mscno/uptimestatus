//! Pure domain model for uptimestatus.
//!
//! Everything in this crate is deterministic and free of I/O. Probes produce an
//! [`Observation`], [`evaluate`] turns it into a [`Verdict`] under a monitor's
//! [`CheckPolicy`], and [`state::apply`] advances the monitor's [`Runtime`].

pub mod api_token;
pub mod auth;
pub mod cert;
pub mod check;
pub mod config;
pub mod domain;
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
pub mod push;
pub mod report;
pub mod routing;
pub mod state;
pub mod status_codes;
pub mod uptime;
pub mod verdict;

pub use api_token::{TOKEN_PREFIX, TokenScope, bearer_token};
pub use auth::{Allowlist, LoginDenied, authorize};
pub use check::{
    CheckSpec, DnsCheck, DnsRecordType, HttpAuth, HttpCheck, HttpMethod, KeywordRule, PushCheck,
    TcpCheck,
};
pub use config::ConfigFile;
pub use domain::{DnsAnswers, Edge, Hostname, HostnameError, points_at_edge};
pub use group::{GroupError, normalize_group, rollup};
pub use incident::{Impact, IncidentKind, IncidentStatus};
pub use json_rule::JsonRule;
pub use maintenance::{MaintenanceError, MaintenanceSpec, Repeat};
pub use monitor::{
    MAX_TAG_LEN, MAX_TAGS, MonitorId, MonitorKey, MonitorKeyError, MonitorSpec, TagError,
    normalize_tags,
};
pub use notify::{
    Alert, AlertEvent, AlertMonitor, ChannelError, ChannelKind, ChannelSpec, format_duration,
};
pub use observation::{FailureKind, Observation, UnknownFailureKind};
pub use page::{
    Accent, BarLevel, ComponentSpec, LayoutError, Look, PageSlug, PageSpec, PageStatus,
    SectionSpec, Theme, Website, bar_level, format_layout, page_status, parse_layout,
};
pub use policy::{CheckPolicy, PolicyError};
pub use push::Push;
pub use report::Report;
pub use routing::{QuietHours, Routing, RoutingError};
pub use state::{MonitorState, Runtime, Step, Transition, UnknownState};
pub use status_codes::{StatusRanges, StatusRangesError};
pub use uptime::{Tally, UptimeWindows};
pub use verdict::{DownReason, Health, UnknownHealth, Verdict, evaluate};
