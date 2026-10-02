//! Runtime configuration.
//!
//! Sources, later ones winning:
//! 1. Built-in defaults.
//! 2. An optional TOML file (`UPTIMESTATUS_CONFIG`, default `uptimestatus.toml`).
//! 3. `UPTIMESTATUS_*` environment variables; nested keys use `__`
//!    (`UPTIMESTATUS_HTTP__PORT=8080`).
//! 4. `DATABASE_URL`, the conventional name platforms inject.

use std::{fmt, net::IpAddr, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use figment::{
    Figment,
    providers::{Env, Format as _, Toml},
};
use serde::Deserialize;
use uptime_domain::Allowlist;
use uptime_web::auth::Key;
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database_url: Secret,
    /// Public URL of the admin console; its host is `APP_HOST`.
    pub app_url: Url,
    /// Hostname custom domains CNAME to (served by your TLS-terminating proxy).
    pub edge_host: String,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub checks: ChecksConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub domains: DomainsConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

/// Where uploaded files (page logos and favicons) live.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// A writable directory (a persistent volume in production). Uploads are disabled
    /// when unset.
    pub path: Option<std::path::PathBuf>,
}

/// Custom-domain verification.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DomainsConfig {
    /// Verify custom domains in this process.
    pub verify: bool,
    /// Public addresses of the edge, for apex domains (A/AAAA records).
    /// A list, or one comma-separated string (`UPTIMESTATUS_DOMAINS__EDGE_IPS`).
    #[serde(deserialize_with = "ip_list")]
    pub edge_ips: Vec<IpAddr>,
    /// How often unverified domains are retried.
    #[serde(with = "humantime_serde")]
    pub verify_every: Duration,
    /// How long a verification holds before DNS is checked again.
    #[serde(with = "humantime_serde")]
    pub recheck_after: Duration,
    /// Request each newly verified domain over HTTPS so the edge obtains its
    /// certificate before the first visitor. Turn off where no edge runs.
    pub prewarm_tls: bool,
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self {
            verify: true,
            edge_ips: Vec::new(),
            verify_every: Duration::from_secs(5 * 60),
            recheck_after: Duration::from_secs(24 * 60 * 60),
            prewarm_tls: true,
        }
    }
}

fn ip_list<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<IpAddr>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        List(Vec<String>),
        One(String),
    }
    let items = match Raw::deserialize(deserializer)? {
        Raw::List(items) => items,
        Raw::One(text) => text.split(',').map(str::to_owned).collect(),
    };
    items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse()
                .map_err(|_| serde::de::Error::custom(format!("`{item}` is not an IP address")))
        })
        .collect()
}

/// Who may sign in to the admin console, and how.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// GitHub usernames allowed to sign in, comma-separated.
    pub admins: String,
    pub github_client_id: Option<String>,
    pub github_client_secret: Option<Secret>,
    /// Base64 of at least 64 random bytes; encrypts the OAuth flow cookie.
    /// Generated per process when unset (fine for a single instance).
    pub cookie_key: Option<Secret>,
    /// Debug builds only: sign in as this user without GitHub.
    pub dev_login: Option<String>,
    /// Sessions end after this much inactivity.
    #[serde(with = "humantime_serde")]
    pub session_idle: Duration,
    /// Sessions end this long after sign-in, however active.
    #[serde(with = "humantime_serde")]
    pub session_max_age: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            admins: String::new(),
            github_client_id: None,
            github_client_secret: None,
            cookie_key: None,
            dev_login: None,
            session_idle: Duration::from_secs(7 * 24 * 60 * 60),
            session_max_age: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

/// The configured cookie key is not usable.
#[derive(Debug, thiserror::Error)]
#[error("auth.cookie_key must be base64 encoding at least 64 bytes")]
pub struct InvalidCookieKey;

impl AuthConfig {
    pub fn allowlist(&self) -> Allowlist {
        self.admins.parse().unwrap_or_default()
    }

    /// The decoded cookie key, if one is configured.
    pub fn cookie_key(&self) -> Result<Option<Key>, InvalidCookieKey> {
        let Some(secret) = &self.cookie_key else {
            return Ok(None);
        };
        let bytes = STANDARD
            .decode(secret.expose().trim())
            .map_err(|_| InvalidCookieKey)?;
        Key::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|_| InvalidCookieKey)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChecksConfig {
    /// Run the scheduler in this process.
    pub enabled: bool,
    /// Checks in flight at once.
    pub concurrency: usize,
    /// How often the scheduler looks for due checks.
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Also probe private and internal addresses. Local development only:
    /// production targets are public, and this would expose the private network.
    pub allow_private_targets: bool,
    /// Dead-man switch pinged while the scheduler is healthy (e.g. healthchecks.io).
    pub heartbeat_url: Option<Url>,
    /// Label recorded with every result (where this instance runs); defaults to `local`.
    pub region: Option<String>,
    /// How long raw check results are kept (daily counters are kept forever).
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            concurrency: 32,
            poll_interval: Duration::from_secs(1),
            allow_private_targets: false,
            heartbeat_url: None,
            region: None,
            retention: Duration::from_secs(90 * 24 * 60 * 60),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// Bind address; `::` accepts IPv4 and IPv6 (some platforms' private networks are IPv6).
    pub host: IpAddr,
    pub port: u16,
    /// Health and readiness; never exposed publicly.
    pub internal_port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::from([0u16; 8]),
            port: 8080,
            internal_port: 9090,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub backend: uptime_store::Backend,
    pub max_connections: usize,
    pub auth_token: Option<Secret>,
    /// File deployments can require a completed import before opening the database.
    pub require_existing: bool,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            backend: uptime_store::Backend::Postgres,
            max_connections: 8,
            auth_token: None,
            require_existing: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub format: LogFormat,
    /// `tracing` filter directives; `RUST_LOG` takes precedence when set.
    pub filter: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per line (production).
    #[default]
    Json,
    /// Human-readable (development).
    Pretty,
}

/// A string that never shows up in logs or `Debug` output.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Config {
    /// Loads configuration from the environment (see module docs).
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Self::figment().extract().map_err(Box::new)
    }

    pub fn figment() -> Figment {
        let file =
            std::env::var("UPTIMESTATUS_CONFIG").unwrap_or_else(|_| "uptimestatus.toml".to_owned());
        Figment::new()
            .merge(Toml::file(file))
            // `UPTIMESTATUS_CONFIG` names the file above; it is not a setting.
            .merge(
                Env::prefixed("UPTIMESTATUS_")
                    .ignore(&["config"])
                    .split("__"),
            )
            .merge(Env::raw().only(&["DATABASE_URL"]))
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail's closure error type is figment's
mod tests {
    use figment::Jail;
    use pretty_assertions::assert_eq;

    use super::*;

    fn required_env(jail: &mut Jail) {
        jail.set_env("DATABASE_URL", "postgres://u:p@db/uptime");
        jail.set_env("UPTIMESTATUS_APP_URL", "https://status.example.com");
        jail.set_env("UPTIMESTATUS_EDGE_HOST", "edge.example.com");
    }

    #[test]
    fn loads_from_environment_with_defaults() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            let config = Config::load().unwrap();
            assert_eq!(config.database_url.expose(), "postgres://u:p@db/uptime");
            assert_eq!(config.app_url.as_str(), "https://status.example.com/");
            assert_eq!(config.edge_host, "edge.example.com");
            assert_eq!(config.http.host, "::".parse::<IpAddr>().unwrap());
            assert_eq!((config.http.port, config.http.internal_port), (8080, 9090));
            assert_eq!(config.database.max_connections, 8);
            assert_eq!(config.database.backend, uptime_store::Backend::Postgres);
            assert_eq!(config.log.format, LogFormat::Json);
            Ok(())
        });
    }

    #[test]
    fn nested_values_come_from_double_underscore_env_vars() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_HTTP__PORT", "3000");
            jail.set_env("UPTIMESTATUS_LOG__FORMAT", "pretty");
            let config = Config::load().unwrap();
            assert_eq!(config.http.port, 3000);
            assert_eq!(config.log.format, LogFormat::Pretty);
            Ok(())
        });
    }

    #[test]
    fn toml_file_is_read_and_env_wins() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "uptimestatus.toml",
                r#"
                    app_url = "https://from-file.example.com"
                    edge_host = "edge.from-file.example.com"
                    [http]
                    port = 7000
                    internal_port = 7001
                "#,
            )?;
            jail.set_env("DATABASE_URL", "postgres://x");
            jail.set_env("UPTIMESTATUS_HTTP__PORT", "7100");
            let config = Config::load().unwrap();
            assert_eq!(config.app_url.as_str(), "https://from-file.example.com/");
            assert_eq!((config.http.port, config.http.internal_port), (7100, 7001));
            Ok(())
        });
    }

    #[test]
    fn missing_required_values_are_reported() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("UPTIMESTATUS_APP_URL", "https://status.example.com");
            let error = Config::load().unwrap_err().to_string();
            assert!(
                error.contains("database_url") || error.contains("edge_host"),
                "{error}"
            );
            Ok(())
        });
    }

    #[test]
    fn domain_settings_have_defaults() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            let domains = Config::load().unwrap().domains;
            assert!(domains.verify && domains.prewarm_tls);
            assert_eq!(domains.edge_ips, Vec::<IpAddr>::new());
            assert_eq!(domains.verify_every, Duration::from_secs(300));
            Ok(())
        });
    }

    #[test]
    fn edge_ips_come_from_a_comma_separated_env_var() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env(
                "UPTIMESTATUS_DOMAINS__EDGE_IPS",
                "203.0.113.10, 2001:db8::10",
            );
            jail.set_env("UPTIMESTATUS_DOMAINS__VERIFY_EVERY", "1m");
            let domains = Config::load().unwrap().domains;
            assert_eq!(
                domains.edge_ips,
                [
                    "203.0.113.10".parse::<IpAddr>().unwrap(),
                    "2001:db8::10".parse().unwrap()
                ]
            );
            assert_eq!(domains.verify_every, Duration::from_secs(60));
            Ok(())
        });
    }

    #[test]
    fn edge_ips_may_be_a_toml_list() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.create_file(
                "uptimestatus.toml",
                r#"
                    [domains]
                    edge_ips = ["203.0.113.10"]
                "#,
            )?;
            assert_eq!(Config::load().unwrap().domains.edge_ips.len(), 1);
            Ok(())
        });
    }

    #[test]
    fn bad_edge_ips_are_rejected() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_DOMAINS__EDGE_IPS", "edge.example.com");
            let error = Config::load().unwrap_err().to_string();
            assert!(error.contains("not an IP address"), "{error}");
            Ok(())
        });
    }

    #[test]
    fn storage_is_optional() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            assert_eq!(Config::load().unwrap().storage.path, None);
            jail.set_env("UPTIMESTATUS_STORAGE__PATH", "/data");
            assert_eq!(
                Config::load().unwrap().storage.path,
                Some(std::path::PathBuf::from("/data"))
            );
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_are_rejected() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_HTTP__PROT", "1");
            assert!(Config::load().is_err());
            Ok(())
        });
    }

    #[test]
    fn database_url_is_redacted_in_debug_output() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            let debug = format!("{:?}", Config::load().unwrap());
            assert!(!debug.contains("u:p@db"), "{debug}");
            assert!(debug.contains("Secret(***)"));
            Ok(())
        });
    }

    #[test]
    fn database_backend_and_turso_token_are_configurable_and_redacted() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("DATABASE_URL", "turso://db.example.turso.io");
            jail.set_env("UPTIMESTATUS_DATABASE__BACKEND", "turso");
            jail.set_env("UPTIMESTATUS_DATABASE__AUTH_TOKEN", "private-token");
            let config = Config::load().unwrap();
            assert_eq!(config.database.backend, uptime_store::Backend::Turso);
            assert_eq!(
                config.database.auth_token.as_ref().map(Secret::expose),
                Some("private-token")
            );
            assert!(!format!("{config:?}").contains("private-token"));
            Ok(())
        });
    }

    #[test]
    fn checks_have_safe_defaults() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            let checks = Config::load().unwrap().checks;
            assert!(checks.enabled);
            assert_eq!(checks.concurrency, 32);
            assert_eq!(checks.poll_interval, std::time::Duration::from_secs(1));
            assert!(
                !checks.allow_private_targets,
                "production refuses private targets"
            );
            assert_eq!(checks.heartbeat_url, None);
            assert_eq!(checks.region, None);
            assert_eq!(
                checks.retention,
                std::time::Duration::from_secs(90 * 24 * 60 * 60)
            );
            Ok(())
        });
    }

    #[test]
    fn checks_are_configurable_from_env() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_CHECKS__ALLOW_PRIVATE_TARGETS", "true");
            jail.set_env("UPTIMESTATUS_CHECKS__POLL_INTERVAL", "500ms");
            jail.set_env(
                "UPTIMESTATUS_CHECKS__HEARTBEAT_URL",
                "https://hc-ping.com/abc",
            );
            jail.set_env("UPTIMESTATUS_CHECKS__RETENTION", "7days");
            let checks = Config::load().unwrap().checks;
            assert_eq!(
                checks.retention,
                std::time::Duration::from_secs(7 * 24 * 60 * 60)
            );
            assert!(checks.allow_private_targets);
            assert_eq!(checks.poll_interval, std::time::Duration::from_millis(500));
            assert_eq!(
                checks.heartbeat_url.unwrap().as_str(),
                "https://hc-ping.com/abc"
            );
            Ok(())
        });
    }

    #[test]
    fn the_config_file_variable_is_not_a_config_key() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.create_file("custom.toml", "edge_host = \"edge.custom.example\"")?;
            jail.set_env("UPTIMESTATUS_CONFIG", "custom.toml");
            let config = Config::figment().extract::<Config>();
            assert!(config.is_ok(), "{config:?}");
            Ok(())
        });
    }

    #[test]
    fn auth_defaults_to_nobody_and_sensible_session_limits() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            let auth = Config::load().unwrap().auth;
            assert!(auth.allowlist().is_empty());
            assert_eq!(auth.github_client_id, None);
            assert_eq!(auth.session_idle, Duration::from_secs(7 * 24 * 60 * 60));
            assert_eq!(auth.session_max_age, Duration::from_secs(30 * 24 * 60 * 60));
            assert!(auth.cookie_key().unwrap().is_none());
            Ok(())
        });
    }

    #[test]
    fn auth_reads_admins_github_and_cookie_key_from_env() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_AUTH__ADMINS", "alice, Bob");
            jail.set_env("UPTIMESTATUS_AUTH__GITHUB_CLIENT_ID", "Iv1.abc");
            jail.set_env("UPTIMESTATUS_AUTH__GITHUB_CLIENT_SECRET", "shh");
            jail.set_env("UPTIMESTATUS_AUTH__COOKIE_KEY", "a".repeat(88));
            let auth = Config::load().unwrap().auth;
            assert!(auth.allowlist().contains("bob"));
            assert_eq!(auth.github_client_id.as_deref(), Some("Iv1.abc"));
            assert_eq!(
                auth.github_client_secret.as_ref().map(Secret::expose),
                Some("shh")
            );
            assert!(auth.cookie_key().unwrap().is_some());
            Ok(())
        });
    }

    #[test]
    fn cookie_keys_must_be_long_enough_base64() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            required_env(jail);
            jail.set_env("UPTIMESTATUS_AUTH__COOKIE_KEY", "c2hvcnQ=");
            let error = Config::load()
                .unwrap()
                .auth
                .cookie_key()
                .unwrap_err()
                .to_string();
            assert!(error.contains("64 bytes"), "{error}");
            Ok(())
        });
    }
}
