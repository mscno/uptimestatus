#![allow(dead_code, unreachable_pub)] // each test binary uses a different subset
#![allow(clippy::unwrap_used)] // test fixtures fail loudly

use std::time::Duration;

use jiff::Timestamp;
use uptime_domain::{CheckPolicy, CheckSpec, HttpCheck, MonitorSpec, TcpCheck};

/// A fixed instant with whole seconds (Postgres stores microseconds).
pub fn t(seconds_after_base: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + seconds_after_base).unwrap()
}

pub fn http_spec(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: format!("Monitor {key}"),
        check: CheckSpec::Http(HttpCheck::get(
            format!("https://{key}.example.com/health").parse().unwrap(),
        )),
        policy: CheckPolicy {
            interval: Duration::from_secs(60),
            retry_interval: Duration::from_secs(20),
            timeout: Duration::from_secs(10),
            retries: 2,
            invert: false,
            degraded_after: Some(Duration::from_millis(1500)),
            resend_every: 3,
        },
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

pub fn tcp_spec(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: format!("TCP {key}"),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 5432)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}
