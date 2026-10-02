//! Conversions between domain types, Toasty models and raw SQL values.

use std::{str::FromStr, time::Duration};

use jiff::Timestamp;
use toasty::stmt::Value;
use uptime_domain::{CheckPolicy, MonitorId, MonitorSpec, MonitorState, Runtime};

use crate::{
    Monitor, Result, RuntimeSnapshot, StoreError,
    models::{MonitorRecord, MonitorRuntimeRecord},
};

/// Milliseconds as stored in `BIGINT` columns (saturating; durations are validated upstream).
pub(crate) fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) fn duration(context: &'static str, millis: i64) -> Result<Duration> {
    u64::try_from(millis)
        .map(Duration::from_millis)
        .map_err(|_| StoreError::corrupt(context, millis))
}

pub(crate) fn count(context: &'static str, value: i32) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::corrupt(context, value))
}

pub(crate) fn parse<T: FromStr>(context: &'static str, value: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|e| StoreError::corrupt(context, e))
}

/// Tags as stored: space-separated, `NULL` when empty.
pub(crate) fn join_tags(tags: &[String]) -> Option<String> {
    (!tags.is_empty()).then(|| tags.join(" "))
}

pub(crate) fn monitor(record: MonitorRecord) -> Result<Monitor> {
    let policy = CheckPolicy {
        interval: duration("monitors.interval_ms", record.interval_ms)?,
        retry_interval: duration("monitors.retry_interval_ms", record.retry_interval_ms)?,
        timeout: duration("monitors.timeout_ms", record.timeout_ms)?,
        retries: count("monitors.retries", record.retries)?,
        invert: record.invert,
        degraded_after: record
            .degraded_after_ms
            .map(|ms| duration("monitors.degraded_after_ms", ms))
            .transpose()?,
        resend_every: count("monitors.resend_every", record.resend_every)?,
    };
    Ok(Monitor {
        id: MonitorId(record.id),
        spec: MonitorSpec {
            key: parse("monitors.key", &record.key)?,
            name: record.name,
            check: record.check.0,
            policy,
            active: record.active,
            tags: record
                .tags
                .as_deref()
                .map(|tags| tags.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            group: record.group_path,
        },
    })
}

pub(crate) fn runtime_snapshot(record: MonitorRuntimeRecord) -> Result<RuntimeSnapshot> {
    Ok(RuntimeSnapshot {
        runtime: Runtime {
            state: parse::<MonitorState>("monitor_runtime.state", &record.state)?,
            consecutive_failures: count(
                "monitor_runtime.consecutive_failures",
                record.consecutive_failures,
            )?,
        },
        next_run_at: record.next_run_at,
        scheduled_for: record.scheduled_for,
        claimed_by: record.claimed_by,
        last_checked_at: record.last_checked_at,
        last_latency_ms: record.last_latency_ms,
        last_status_code: record
            .last_status_code
            .map(|code| {
                u16::try_from(code)
                    .map_err(|_| StoreError::corrupt("monitor_runtime.last_status_code", code))
            })
            .transpose()?,
        last_error: record.last_error,
        state_changed_at: record.state_changed_at,
        cert_expires_at: record.cert_expires_at,
    })
}

/// Field accessors for rows returned by raw SQL queries.
pub(crate) struct Row<'a> {
    context: &'static str,
    fields: &'a [Value],
}

impl<'a> Row<'a> {
    pub(crate) fn new(context: &'static str, value: &'a Value) -> Result<Self> {
        match value {
            Value::Record(record) => Ok(Self {
                context,
                fields: record.as_slice(),
            }),
            other => Err(StoreError::corrupt(
                context,
                format!("expected a row, got {other:?}"),
            )),
        }
    }

    fn field(&self, index: usize) -> Result<&'a Value> {
        self.fields
            .get(index)
            .ok_or_else(|| StoreError::corrupt(self.context, format!("missing column {index}")))
    }

    pub(crate) fn i64(&self, index: usize) -> Result<i64> {
        match self.field(index)? {
            Value::I64(v) => Ok(*v),
            Value::I32(v) => Ok(i64::from(*v)),
            other => Err(StoreError::corrupt(
                self.context,
                format!("column {index}: expected integer, got {other:?}"),
            )),
        }
    }

    pub(crate) fn string(&self, index: usize) -> Result<&'a str> {
        match self.field(index)? {
            Value::String(v) => Ok(v),
            other => Err(StoreError::corrupt(
                self.context,
                format!("column {index}: expected text, got {other:?}"),
            )),
        }
    }

    pub(crate) fn timestamp(&self, index: usize) -> Result<Timestamp> {
        match self.field(index)? {
            Value::Timestamp(v) => Ok(*v),
            Value::String(v) => parse(self.context, v),
            other => Err(StoreError::corrupt(
                self.context,
                format!("column {index}: expected timestamp, got {other:?}"),
            )),
        }
    }
}
