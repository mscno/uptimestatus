//! Checks that speak a service's own protocol: ICMP ping, database queries,
//! gRPC health and MQTT.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use url::Url;

/// Why a check's settings cannot work.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("enter a hostname or IP address, without a scheme")]
    Host,
    #[error("send between 1 and {max} packets", max = PingCheck::MAX_COUNT)]
    PingCount,
    #[error("the packet timeout must be between 100ms and 10s")]
    PacketTimeout,
    #[error("the packet loss threshold must be between 1% and 100%")]
    LossThreshold,
    #[error("use a postgres://, mysql:// or redis:// (rediss:// for TLS) URL with a host")]
    DatabaseUrl,
    #[error("use an http:// or https:// URL with a host and no path")]
    GrpcUrl,
    #[error("metadata names are lowercase letters, digits, `-`, `_` and `.`")]
    Metadata,
    #[error("the port must be between 1 and 65535")]
    Port,
    #[error("an expected payload needs a topic to subscribe to")]
    ExpectWithoutTopic,
    #[error("topics to subscribe to may use `+` and `#` wildcards but not be empty")]
    Topic,
}

/// ICMP echo requests; UP while enough of them are answered.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PingCheck {
    pub host: String,
    /// Echo requests sent per check.
    #[serde(
        default = "PingCheck::default_count",
        skip_serializing_if = "PingCheck::is_default_count"
    )]
    pub count: u8,
    /// How long to wait for each reply.
    #[serde(
        default = "PingCheck::default_packet_timeout",
        with = "humantime_serde",
        skip_serializing_if = "PingCheck::is_default_packet_timeout"
    )]
    pub packet_timeout: Duration,
    /// DOWN when at least this share of the packets is lost (100: only when
    /// every packet is lost).
    #[serde(
        default = "PingCheck::default_loss_threshold",
        skip_serializing_if = "PingCheck::is_default_loss_threshold"
    )]
    pub loss_threshold_percent: u8,
}

impl fmt::Debug for PingCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PingCheck")
            .field("host", &self.host)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl PingCheck {
    pub const MAX_COUNT: u8 = 10;

    const fn default_count() -> u8 {
        3
    }

    fn is_default_count(count: &u8) -> bool {
        *count == Self::default_count()
    }

    const fn default_packet_timeout() -> Duration {
        Duration::from_secs(1)
    }

    fn is_default_packet_timeout(timeout: &Duration) -> bool {
        *timeout == Self::default_packet_timeout()
    }

    const fn default_loss_threshold() -> u8 {
        100
    }

    fn is_default_loss_threshold(percent: &u8) -> bool {
        *percent == Self::default_loss_threshold()
    }

    /// Three packets, one second each, DOWN only when all are lost.
    pub fn host(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            count: Self::default_count(),
            packet_timeout: Self::default_packet_timeout(),
            loss_threshold_percent: Self::default_loss_threshold(),
        }
    }

    pub fn validate(&self) -> Result<(), CheckError> {
        if !valid_host(&self.host) {
            return Err(CheckError::Host);
        }
        if !(1..=Self::MAX_COUNT).contains(&self.count) {
            return Err(CheckError::PingCount);
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(10)).contains(&self.packet_timeout) {
            return Err(CheckError::PacketTimeout);
        }
        if !(1..=100).contains(&self.loss_threshold_percent) {
            return Err(CheckError::LossThreshold);
        }
        Ok(())
    }

    /// Whether losing `lost` of `sent` packets makes the target DOWN.
    pub fn too_lossy(&self, lost: u32, sent: u32) -> bool {
        sent == 0 || lost * 100 >= u32::from(self.loss_threshold_percent) * sent
    }
}

/// Which database a [`DatabaseCheck`] talks to, from its URL scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DatabaseEngine {
    Postgres,
    Mysql,
    Redis,
}

impl DatabaseEngine {
    pub fn label(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Redis => "Redis",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Mysql => 3306,
            Self::Redis => 6379,
        }
    }

    /// What runs when no query is given.
    pub fn default_query(self) -> &'static str {
        match self {
            Self::Postgres | Self::Mysql => "SELECT 1",
            Self::Redis => "PING",
        }
    }
}

/// Connects to a database and runs a query; UP when it succeeds (and the
/// first value contains `expect`, if given).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseCheck {
    /// `postgres://`, `mysql://`, `redis://` or `rediss://`, with credentials.
    /// Secret: never shown in logs, errors or read-only API responses.
    pub url: String,
    /// SQL, or a Redis command such as `GET health`; blank runs `SELECT 1` / `PING`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Text the first value of the result must contain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// TLS only: accept certificates that do not verify.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_tls_errors: bool,
}

impl fmt::Debug for DatabaseCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseCheck")
            .field("engine", &self.engine())
            .field("host", &self.host())
            .finish_non_exhaustive()
    }
}

impl DatabaseCheck {
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            query: None,
            expect: None,
            ignore_tls_errors: false,
        }
    }

    fn parsed(&self) -> Option<Url> {
        Url::parse(self.url.trim()).ok()
    }

    /// The engine its URL names.
    pub fn engine(&self) -> Option<DatabaseEngine> {
        match self.parsed()?.scheme() {
            "postgres" | "postgresql" => Some(DatabaseEngine::Postgres),
            "mysql" | "mariadb" => Some(DatabaseEngine::Mysql),
            "redis" | "rediss" => Some(DatabaseEngine::Redis),
            _ => None,
        }
    }

    pub fn host(&self) -> Option<String> {
        self.parsed()?
            .host_str()
            .filter(|host| !host.is_empty())
            .map(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned()
            })
    }

    pub fn port(&self) -> Option<u16> {
        let url = self.parsed()?;
        url.port().or_else(|| Some(self.engine()?.default_port()))
    }

    /// What runs: the query, or the engine's default.
    pub fn query_text(&self) -> Option<String> {
        let query = self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        Some(match query {
            Some(query) => query.to_owned(),
            None => self.engine()?.default_query().to_owned(),
        })
    }

    /// `scheme://host:port`, without credentials or database name.
    pub fn display_target(&self) -> String {
        match (self.parsed(), self.engine(), self.host(), self.port()) {
            (Some(url), Some(_), Some(host), Some(port)) => {
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host
                };
                format!("{}://{host}:{port}", url.scheme())
            }
            _ => "invalid database URL".to_owned(),
        }
    }

    pub fn validate(&self) -> Result<(), CheckError> {
        match (self.engine(), self.host()) {
            (Some(_), Some(_)) if self.port().is_some_and(|p| p > 0) => Ok(()),
            _ => Err(CheckError::DatabaseUrl),
        }
    }
}

/// The standard gRPC health service (`grpc.health.v1.Health/Check`); UP when
/// it answers SERVING.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcCheck {
    /// `http://host:port` (plaintext HTTP/2) or `https://host:port`.
    pub url: Url,
    /// The service to ask about; blank asks about the whole server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Request metadata (headers) as `(name, value)` pairs; values may be secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_tls_errors: bool,
}

impl fmt::Debug for GrpcCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrpcCheck")
            .field("host", &self.url.host_str())
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl GrpcCheck {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            service: None,
            metadata: Vec::new(),
            ignore_tls_errors: false,
        }
    }

    pub fn tls(&self) -> bool {
        self.url.scheme() == "https"
    }

    pub fn port(&self) -> u16 {
        self.url
            .port_or_known_default()
            .unwrap_or(if self.tls() { 443 } else { 80 })
    }

    pub fn validate(&self) -> Result<(), CheckError> {
        let url = &self.url;
        let ok = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|h| !h.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none();
        if !ok {
            return Err(CheckError::GrpcUrl);
        }
        let valid_name = |name: &str| {
            !name.is_empty()
                && !name.ends_with("-bin")
                && !name.starts_with("grpc-")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
        };
        let valid_value = |value: &str| value.bytes().all(|b| (0x20..0x7f).contains(&b));
        if self
            .metadata
            .iter()
            .any(|(name, value)| !valid_name(name) || !valid_value(value))
        {
            return Err(CheckError::Metadata);
        }
        Ok(())
    }
}

/// Connects to an MQTT broker; UP on a successful CONNACK, or, with a topic,
/// when a message arrives on it (containing `expect`, if given).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttCheck {
    pub host: String,
    /// Defaults to 1883, or 8883 with TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_tls_errors: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Prefix of the client id; a random suffix keeps checks from colliding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Subscribe and wait for a message (retained messages arrive at once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Text the message payload must contain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

impl fmt::Debug for MqttCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttCheck")
            .field("host", &self.host)
            .field("port", &self.port())
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl MqttCheck {
    pub fn connect(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            tls: false,
            ignore_tls_errors: false,
            username: None,
            password: None,
            client_id: None,
            topic: None,
            expect: None,
        }
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(if self.tls { 8883 } else { 1883 })
    }

    pub fn validate(&self) -> Result<(), CheckError> {
        if !valid_host(&self.host) {
            return Err(CheckError::Host);
        }
        if self.port == Some(0) {
            return Err(CheckError::Port);
        }
        let topic = self.topic.as_deref().map(str::trim);
        if topic.is_some_and(str::is_empty) {
            return Err(CheckError::Topic);
        }
        if self.expect.as_deref().is_some_and(|e| !e.is_empty()) && topic.is_none() {
            return Err(CheckError::ExpectWithoutTopic);
        }
        Ok(())
    }
}

/// A hostname or IP literal: no scheme, port or whitespace.
fn valid_host(host: &str) -> bool {
    let host = host.trim();
    !host.is_empty()
        && !host.contains(char::is_whitespace)
        && !host.contains('/')
        && (host.parse::<std::net::IpAddr>().is_ok() || !host.contains(':'))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn ping_defaults_are_compact_and_sensible() {
        let check = PingCheck::host("example.com");
        assert_eq!(
            serde_json::to_value(&check).unwrap(),
            serde_json::json!({"host": "example.com"})
        );
        assert_eq!(check.validate(), Ok(()));
        assert!(!check.too_lossy(2, 3), "one reply is enough by default");
        assert!(check.too_lossy(3, 3));
        let strict = PingCheck {
            loss_threshold_percent: 50,
            packet_timeout: Duration::from_millis(500),
            ..check
        };
        assert!(strict.too_lossy(2, 4) && !strict.too_lossy(1, 4));
        let json = serde_json::to_value(&strict).unwrap();
        assert_eq!(json["packet_timeout"], "500ms");
        assert_eq!(serde_json::from_value::<PingCheck>(json).unwrap(), strict);
    }

    #[rstest]
    #[case(PingCheck { count: 0, ..PingCheck::host("h") }, CheckError::PingCount)]
    #[case(PingCheck { count: 11, ..PingCheck::host("h") }, CheckError::PingCount)]
    #[case(PingCheck { packet_timeout: Duration::from_millis(10), ..PingCheck::host("h") }, CheckError::PacketTimeout)]
    #[case(PingCheck { loss_threshold_percent: 0, ..PingCheck::host("h") }, CheckError::LossThreshold)]
    #[case(PingCheck::host("https://example.com"), CheckError::Host)]
    #[case(PingCheck::host("example.com:80"), CheckError::Host)]
    fn ping_rejects_unworkable_settings(#[case] check: PingCheck, #[case] error: CheckError) {
        assert_eq!(check.validate(), Err(error));
    }

    #[test]
    fn ping_accepts_ipv6_literals() {
        assert_eq!(PingCheck::host("2606:4700:4700::1111").validate(), Ok(()));
    }

    #[rstest]
    #[case(
        "postgres://u:p@db.example.com/app",
        DatabaseEngine::Postgres,
        "postgres://db.example.com:5432"
    )]
    #[case(
        "postgresql://db.example.com:6543",
        DatabaseEngine::Postgres,
        "postgresql://db.example.com:6543"
    )]
    #[case(
        "mysql://root:secret@10.0.0.1/shop",
        DatabaseEngine::Mysql,
        "mysql://10.0.0.1:3306"
    )]
    #[case(
        "rediss://:pw@cache.example.com",
        DatabaseEngine::Redis,
        "rediss://cache.example.com:6379"
    )]
    #[case("redis://[::1]:7000/2", DatabaseEngine::Redis, "redis://[::1]:7000")]
    fn database_urls_name_their_engine_without_credentials(
        #[case] url: &str,
        #[case] engine: DatabaseEngine,
        #[case] shown: &str,
    ) {
        let check = DatabaseCheck::url(url);
        assert_eq!(check.engine(), Some(engine));
        assert_eq!(check.display_target(), shown);
        assert_eq!(check.validate(), Ok(()));
        assert!(!format!("{check:?}").contains("secret"));
    }

    #[rstest]
    #[case("mongodb://db.example.com")]
    #[case("postgres://")]
    #[case("db.example.com:5432")]
    fn database_checks_need_a_supported_url(#[case] url: &str) {
        assert_eq!(
            DatabaseCheck::url(url).validate(),
            Err(CheckError::DatabaseUrl)
        );
    }

    #[test]
    fn database_queries_default_per_engine() {
        assert_eq!(
            DatabaseCheck::url("redis://cache").query_text().as_deref(),
            Some("PING")
        );
        let custom = DatabaseCheck {
            query: Some(" SELECT version() ".into()),
            ..DatabaseCheck::url("postgres://db")
        };
        assert_eq!(custom.query_text().as_deref(), Some("SELECT version()"));
    }

    #[test]
    fn grpc_checks_validate_their_url_and_metadata() {
        let check = GrpcCheck {
            metadata: vec![("authorization".into(), "Bearer secret".into())],
            ..GrpcCheck::new("https://grpc.example.com".parse().unwrap())
        };
        assert_eq!(check.validate(), Ok(()));
        assert!(check.tls());
        assert_eq!(check.port(), 443);
        assert!(!format!("{check:?}").contains("secret"));
        for url in [
            "ftp://grpc.example.com",
            "https://grpc.example.com/health",
            "https://u:p@h",
        ] {
            assert_eq!(
                GrpcCheck::new(url.parse().unwrap()).validate(),
                Err(CheckError::GrpcUrl),
                "{url}"
            );
        }
        let bad = GrpcCheck {
            metadata: vec![("X-Upper".into(), "v".into())],
            ..GrpcCheck::new("http://h:50051".parse().unwrap())
        };
        assert_eq!(bad.validate(), Err(CheckError::Metadata));
    }

    #[test]
    fn mqtt_checks_default_their_port_and_need_a_topic_to_expect() {
        let plain = MqttCheck::connect("broker.example.com");
        assert_eq!(plain.port(), 1883);
        assert_eq!(
            MqttCheck {
                tls: true,
                ..plain.clone()
            }
            .port(),
            8883
        );
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"host": "broker.example.com"})
        );
        let expecting = MqttCheck {
            expect: Some("ok".into()),
            ..plain.clone()
        };
        assert_eq!(expecting.validate(), Err(CheckError::ExpectWithoutTopic));
        let secret = MqttCheck {
            password: Some("hunter2".into()),
            ..plain
        };
        assert!(!format!("{secret:?}").contains("hunter2"));
    }
}
