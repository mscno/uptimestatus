use uptime_domain::{Hostname, MonitorId, MonitorKey, PageSlug};

/// Errors from the persistence layer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] toasty::Error),

    #[error("invalid database configuration: {0}")]
    Configuration(String),

    #[error("a monitor with key `{0}` already exists")]
    DuplicateKey(MonitorKey),

    #[error("monitor {0} not found")]
    MonitorNotFound(MonitorId),

    #[error("a status page with slug `{0}` already exists")]
    DuplicateSlug(PageSlug),

    #[error("status page {0} not found")]
    PageNotFound(i64),

    #[error("`{0}` is already used by a status page")]
    DuplicateDomain(Hostname),

    #[error("notification channel {0} not found")]
    ChannelNotFound(i64),

    #[error("incident {0} not found")]
    IncidentNotFound(i64),

    #[error("maintenance window {0} not found")]
    MaintenanceNotFound(i64),

    #[error("custom domain {0} not found")]
    DomainNotFound(i64),

    #[error("no monitors with the keys {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))]
    UnknownMonitors(Vec<MonitorKey>),

    /// A row did not match the shape the code expects (schema drift or bad data).
    #[error("unexpected data in {context}: {detail}")]
    Corrupt {
        context: &'static str,
        detail: String,
    },
}

impl StoreError {
    pub(crate) fn corrupt(context: &'static str, detail: impl std::fmt::Display) -> Self {
        Self::Corrupt {
            context,
            detail: detail.to_string(),
        }
    }
}
