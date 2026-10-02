//! Logging and tracing setup.
//!
//! Production logs are JSON, one object per line: the event's fields at the
//! top level, plus `span` with the fields of the innermost span (the monitor
//! being checked, the alert channel, the request id), so every line says
//! what it is about.

use tracing_subscriber::{
    EnvFilter, Layer, fmt, fmt::MakeWriter, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use crate::config::{LogConfig, LogFormat};

/// Installs the global `tracing` subscriber and routes panics through it.
/// `RUST_LOG` overrides the configured filter.
pub fn init(config: &LogConfig) -> anyhow::Result<()> {
    let filter = filter(config, std::env::var("RUST_LOG").ok().as_deref())?;
    let registry = tracing_subscriber::registry().with(filter);
    match config.format {
        LogFormat::Json => registry.with(json_layer(std::io::stdout)).try_init()?,
        LogFormat::Pretty => registry.with(fmt::layer().with_target(false)).try_init()?,
    }
    log_panics();
    Ok(())
}

/// JSON lines: flattened event fields and the current span's fields.
fn json_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(writer)
}

/// Logs panics as errors (with where they happened) instead of printing
/// them to stderr outside the log format.
fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(no message)");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        let backtrace = std::backtrace::Backtrace::capture();
        let backtrace = (backtrace.status() == std::backtrace::BacktraceStatus::Captured)
            .then(|| backtrace.to_string());
        tracing::error!(
            panic = message,
            location = location.as_deref(),
            backtrace = backtrace.as_deref(),
            "panicked"
        );
    }));
}

/// Default directives: `info`, minus the database driver's chatter (Postgres
/// `NOTICE`s such as "relation already exists").
const DEFAULT_FILTER: &str = "info,tokio_postgres=warn";

/// Resolves filter directives: `RUST_LOG`, then the config, then [`DEFAULT_FILTER`].
fn filter(config: &LogConfig, rust_log: Option<&str>) -> anyhow::Result<EnvFilter> {
    let directives = rust_log
        .filter(|directives| !directives.trim().is_empty())
        .or(config.filter.as_deref())
        .unwrap_or(DEFAULT_FILTER);
    Ok(EnvFilter::try_new(directives)?)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn config(filter: Option<&str>) -> LogConfig {
        LogConfig {
            format: LogFormat::Json,
            filter: filter.map(str::to_owned),
        }
    }

    #[test]
    fn rust_log_wins_over_config() {
        let filter = filter(&config(Some("warn")), Some("debug")).unwrap();
        assert_eq!(filter.to_string(), "debug");
    }

    #[test]
    fn config_filter_applies_without_rust_log() {
        assert_eq!(
            filter(&config(Some("warn")), None).unwrap().to_string(),
            "warn"
        );
        assert_eq!(
            filter(&config(Some("warn")), Some("  "))
                .unwrap()
                .to_string(),
            "warn"
        );
    }

    #[test]
    fn defaults_to_info_without_driver_chatter() {
        let directives = filter(&config(None), None).unwrap().to_string();
        assert!(directives.contains("tokio_postgres=warn"), "{directives}");
        assert!(directives.contains("info"), "{directives}");
    }

    #[test]
    fn rejects_invalid_directives() {
        assert!(filter(&config(Some("=[")), None).is_err());
    }

    #[test]
    fn json_lines_say_what_they_are_about() {
        let logs = uptime_testkit::logs::Logs::default();
        let subscriber = tracing_subscriber::registry().with(json_layer(logs.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("execute", key = "api", monitor = 3);
            let _entered = span.enter();
            tracing::warn!(reason = "timeout", "check failed");
        });
        let line = logs.lines().remove(0);
        assert_eq!(line["message"], "check failed");
        assert_eq!(line["reason"], "timeout");
        assert_eq!(line["span"]["key"], "api");
        assert_eq!(line["span"]["monitor"], 3);
        assert!(line.get("spans").is_none(), "no span list: {line}");
    }

    // nextest runs every test in its own process, so this gets a fresh global subscriber.
    #[test]
    fn installs_the_global_subscriber_once() {
        let config = config(Some("debug"));
        init(&config).unwrap();
        assert!(
            init(&config).is_err(),
            "a second global subscriber is rejected"
        );
    }
}
