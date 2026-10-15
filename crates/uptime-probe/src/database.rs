//! Database probe: connect, run one query, report its first value.
//!
//! Every connection goes to an address the [`AddressPolicy`](crate::AddressPolicy)
//! permits: PostgreSQL and Redis speak over a socket this module opens; MySQL
//! is pointed at the resolved IP, with TLS still verified against the hostname.
//! Connection URLs carry credentials, so they never appear in a failure message.

use std::time::{Duration, Instant};

use redis::aio::MultiplexedConnection;
use tokio::net::TcpStream;
use tokio_postgres::{SimpleQueryMessage, config::SslMode, tls::MakeTlsConnect};
use tokio_util::either::Either;
use uptime_domain::{DatabaseCheck, DatabaseEngine, FailureKind, Observation};
use url::Url;

use crate::{
    excerpt,
    resolve::PolicyResolver,
    tcp::{self, Failure},
};

/// What a query returned.
struct Answer {
    latency: Duration,
    /// The first value, as text.
    value: String,
}

pub(crate) async fn probe(
    resolver: &PolicyResolver,
    check: &DatabaseCheck,
    timeout: Duration,
) -> Observation {
    let started = Instant::now();
    let attempt = async {
        let engine = check.engine().ok_or_else(invalid_url)?;
        let host = check.host().ok_or_else(invalid_url)?;
        let port = check.port().ok_or_else(invalid_url)?;
        let query = check.query_text().ok_or_else(invalid_url)?;
        let value = match engine {
            DatabaseEngine::Postgres => postgres(resolver, check, &host, port, &query).await?,
            DatabaseEngine::Mysql => mysql(resolver, check, &host, port, &query).await?,
            DatabaseEngine::Redis => redis(resolver, check, &host, port, &query).await?,
        };
        Ok::<_, Failure>(Answer {
            latency: started.elapsed(),
            value,
        })
    };
    match tokio::time::timeout(timeout, attempt).await {
        Ok(Ok(answer)) => Observation::Responded {
            latency: answer.latency,
            status_code: None,
            keyword_found: check
                .expect
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|expect| answer.value.contains(expect)),
            json_matched: None,
            cert_expires_at: None,
            response_body: Some(excerpt(answer.value)),
        },
        Ok(Err((kind, message))) => Observation::Failed { kind, message },
        Err(_) => Observation::Failed {
            kind: FailureKind::Timeout,
            message: format!("no answer within {timeout:?}"),
        },
    }
}

fn invalid_url() -> Failure {
    (
        FailureKind::Io,
        "invalid connection URL: use postgres://, mysql:// or redis:// with a host".to_owned(),
    )
}

// ── PostgreSQL ───────────────────────────────────────────────────────────

/// Options libpq understands but tokio-postgres does not; certificates are
/// verified against the platform trust store instead.
const LIBPQ_ONLY: [&str; 5] = [
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslpassword",
    "sslnegotiation",
];

/// The URL as tokio-postgres reads it: `verify-*` modes become `require`
/// (which, here, always verifies unless errors are ignored).
fn postgres_config(url: &str) -> Result<tokio_postgres::Config, Failure> {
    let mut url = Url::parse(url).map_err(|_| invalid_url())?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !LIBPQ_ONLY.contains(&key.as_ref()))
        .map(|(key, value)| {
            let value = match (key.as_ref(), value.as_ref()) {
                ("sslmode", "verify-ca" | "verify-full") => "require".to_owned(),
                ("sslmode", "allow") => "prefer".to_owned(),
                ("sslmode", "disable" | "prefer" | "require") => value.into_owned(),
                ("sslmode", other) => {
                    return Err((
                        FailureKind::Io,
                        format!("invalid connection URL: unsupported sslmode `{other}`"),
                    ));
                }
                _ => value.into_owned(),
            };
            Ok((key.into_owned(), value))
        })
        .collect::<Result<_, Failure>>()?;
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    url.as_str()
        .parse::<tokio_postgres::Config>()
        .map_err(|error| {
            // Option errors name the option, never its value or the URL.
            (FailureKind::Io, format!("invalid connection URL: {error}"))
        })
}

async fn postgres(
    resolver: &PolicyResolver,
    check: &DatabaseCheck,
    host: &str,
    port: u16,
    query: &str,
) -> Result<String, Failure> {
    let mut config = postgres_config(&check.url)?;
    config.connect_timeout(Duration::from_secs(30));
    let stream = tcp::connect(resolver, host, port).await?;
    let tls_config = tcp::tls_config(check.ignore_tls_errors)?;
    let mut make = tokio_postgres_rustls::MakeRustlsConnect::new((*tls_config).clone());
    let tls = MakeTlsConnect::<TcpStream>::make_tls_connect(&mut make, host)
        .map_err(|e| (FailureKind::Tls, format!("invalid server name: {e}")))?;
    let (client, connection) = config
        .connect_raw(stream, tls)
        .await
        .map_err(|e| postgres_failure(&e, config.get_ssl_mode()))?;
    let driver = tokio::spawn(connection);
    let result = client.simple_query(query).await;
    drop(client);
    driver.abort();
    let messages = result.map_err(|e| postgres_failure(&e, config.get_ssl_mode()))?;
    Ok(messages
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or("NULL").to_owned()),
            _ => None,
        })
        .unwrap_or_default())
}

fn postgres_failure(error: &tokio_postgres::Error, mode: SslMode) -> Failure {
    let text = error_chain(error);
    let kind = if let Some(db) = error.as_db_error() {
        // Authentication, unknown database, permission or SQL errors.
        return (
            FailureKind::Io,
            format!("{}: {}", db.code().code(), db.message()),
        );
    } else if text.contains("tls") || text.contains("TLS") || text.contains("certificate") {
        FailureKind::Tls
    } else if text.contains("refused") || text.contains("reset") {
        FailureKind::Refused
    } else if mode == SslMode::Require && text.contains("server does not support TLS") {
        FailureKind::Tls
    } else {
        FailureKind::Io
    };
    (kind, text)
}

// ── MySQL ────────────────────────────────────────────────────────────────

async fn mysql(
    resolver: &PolicyResolver,
    check: &DatabaseCheck,
    host: &str,
    port: u16,
    query: &str,
) -> Result<String, Failure> {
    let mut url = Url::parse(&check.url).map_err(|_| invalid_url())?;
    if url.scheme() == "mariadb" {
        url.set_scheme("mysql").map_err(|()| invalid_url())?;
    }
    let opts = mysql_async::Opts::from_url(url.as_str()).map_err(|error| {
        let reason = match error {
            mysql_async::UrlError::UnknownParameter { param } => {
                format!("unknown parameter `{param}`")
            }
            mysql_async::UrlError::InvalidParamValue { param, .. } => {
                format!("invalid value for `{param}`")
            }
            _ => "check the scheme, host and parameters".to_owned(),
        };
        (FailureKind::Io, format!("invalid connection URL: {reason}"))
    })?;
    // Connect to an address the policy permits; TLS still checks the hostname.
    let ip = *resolver
        .resolve(host)
        .await
        .map_err(tcp::resolve_failure)?
        .first()
        .ok_or_else(invalid_url)?;
    let ssl = opts.ssl_opts().cloned().map(|ssl| {
        ssl.with_danger_tls_hostname_override(Some(host.to_owned()))
            .with_danger_accept_invalid_certs(check.ignore_tls_errors)
            .with_danger_skip_domain_validation(check.ignore_tls_errors)
    });
    let opts = mysql_async::OptsBuilder::from_opts(opts)
        .ip_or_hostname(ip.to_string())
        .tcp_port(port)
        .prefer_socket(false)
        .ssl_opts(ssl);
    let mut conn = mysql_async::Conn::new(opts).await.map_err(mysql_failure)?;
    use mysql_async::prelude::Queryable as _;
    let row: Option<mysql_async::Row> = conn.query_first(query).await.map_err(mysql_failure)?;
    let _ = conn.disconnect().await;
    Ok(row
        .and_then(|row| row.as_ref(0).cloned())
        .map(mysql_text)
        .unwrap_or_default())
}

fn mysql_text(value: mysql_async::Value) -> String {
    use mysql_async::Value;
    match value {
        Value::NULL => "NULL".to_owned(),
        Value::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        other => other.as_sql(true),
    }
}

fn mysql_failure(error: mysql_async::Error) -> Failure {
    use mysql_async::Error;
    match &error {
        Error::Io(io) => {
            let text = io.to_string();
            let kind =
                if text.contains("TLS") || text.contains("tls") || text.contains("certificate") {
                    FailureKind::Tls
                } else if text.contains("refused") || text.contains("reset") {
                    FailureKind::Refused
                } else {
                    FailureKind::Io
                };
            (kind, text)
        }
        Error::Server(server) => (
            FailureKind::Io,
            format!("{}: {}", server.code, server.message),
        ),
        // Driver and URL errors can quote connection options.
        Error::Driver(driver) => (FailureKind::Io, driver.to_string()),
        Error::Url(_) => invalid_url(),
        Error::Other(other) => (FailureKind::Io, other.to_string()),
    }
}

// ── Redis ────────────────────────────────────────────────────────────────

async fn redis(
    resolver: &PolicyResolver,
    check: &DatabaseCheck,
    host: &str,
    port: u16,
    query: &str,
) -> Result<String, Failure> {
    let url = Url::parse(&check.url).map_err(|_| invalid_url())?;
    let mut info = redis::RedisConnectionInfo::default();
    if !url.username().is_empty() {
        info = info.set_username(percent_decode(url.username()));
    }
    if let Some(password) = url.password() {
        info = info.set_password(percent_decode(password));
    }
    if let Some(db) = url
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|d| !d.is_empty())
    {
        let db = db.parse::<i64>().map_err(|_| {
            (
                FailureKind::Io,
                "invalid connection URL: the path must be a database number".to_owned(),
            )
        })?;
        info = info.set_db(db);
    }
    let args = split_command(query).ok_or((
        FailureKind::Io,
        "the Redis command has an unclosed quote".to_owned(),
    ))?;
    let stream = tcp::connect(resolver, host, port).await?;
    let stream = if url.scheme() == "rediss" {
        Either::Right(tcp::tls_connect(stream, host, check.ignore_tls_errors, &[]).await?)
    } else {
        Either::Left(stream)
    };
    let config = redis::AsyncConnectionConfig::new();
    let (mut conn, driver) = MultiplexedConnection::new_with_config(&info, stream, config)
        .await
        .map_err(|e| redis_failure(&e))?;
    let driver = tokio::spawn(driver);
    let mut command = redis::cmd(&args[0]);
    for arg in &args[1..] {
        command.arg(arg);
    }
    let result = command.query_async::<redis::Value>(&mut conn).await;
    drop(conn);
    driver.abort();
    Ok(redis_text(&result.map_err(|e| redis_failure(&e))?))
}

/// The reply as text; for arrays, the first element.
fn redis_text(value: &redis::Value) -> String {
    use redis::Value;
    match value {
        Value::Nil => "(nil)".to_owned(),
        Value::Int(n) => n.to_string(),
        Value::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::SimpleString(text) => text.clone(),
        Value::Okay => "OK".to_owned(),
        Value::Array(items) | Value::Set(items) => {
            items.first().map(redis_text).unwrap_or_default()
        }
        Value::Double(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::VerbatimString { text, .. } => text.clone(),
        other => format!("{other:?}"),
    }
}

fn redis_failure(error: &redis::RedisError) -> Failure {
    let text = error.to_string();
    let kind = if error.is_connection_refusal() {
        FailureKind::Refused
    } else if error.is_timeout() {
        FailureKind::Timeout
    } else {
        FailureKind::Io
    };
    (kind, text)
}

/// `GET "my key"` → `["GET", "my key"]`; `None` on an unclosed quote.
fn split_command(text: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(current);
    }
    (!args.is_empty()).then_some(args)
}

/// URL userinfo with `%XX` escapes resolved (`+` stays a plus).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or(b'?'));
                i += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An error and its causes, deduplicated.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cause = Some(error);
    while let Some(current) = cause {
        let text = current.to_string();
        if !parts.iter().any(|seen| seen.contains(&text)) {
            parts.push(text);
        }
        cause = current.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn commands_split_on_whitespace_and_respect_quotes() {
        assert_eq!(split_command("PING"), Some(vec!["PING".to_owned()]));
        assert_eq!(
            split_command(r#"GET "health key"  'x y'"#),
            Some(vec![
                "GET".to_owned(),
                "health key".to_owned(),
                "x y".to_owned()
            ])
        );
        assert_eq!(split_command(r#"GET "open"#), None);
        assert_eq!(split_command("   "), None);
        assert_eq!(
            split_command(r#"SET k """#),
            Some(vec!["SET".into(), "k".into(), String::new()])
        );
    }

    #[test]
    fn libpq_urls_are_read_by_tokio_postgres() {
        let config = postgres_config(
            "postgres://u:p%40ss@db.example.com:5433/app?sslmode=verify-full&sslrootcert=system",
        )
        .unwrap();
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
        assert_eq!(config.get_user(), Some("u"));
        assert_eq!(config.get_password(), Some(&b"p@ss"[..]));
        assert_eq!(config.get_dbname(), Some("app"));
    }

    #[test]
    fn invalid_postgres_options_never_echo_the_url() {
        let (_, message) =
            postgres_config("postgres://u:topsecret@db/app?sslmode=sometimes").unwrap_err();
        assert!(!message.contains("topsecret"), "{message}");
        assert!(message.contains("sslmode"), "{message}");
    }

    #[test]
    fn redis_replies_become_text() {
        use redis::Value;
        assert_eq!(redis_text(&Value::SimpleString("PONG".into())), "PONG");
        assert_eq!(redis_text(&Value::BulkString(b"ok".to_vec())), "ok");
        assert_eq!(redis_text(&Value::Nil), "(nil)");
        assert_eq!(
            redis_text(&Value::Array(vec![Value::Int(7), Value::Int(8)])),
            "7"
        );
    }

    #[test]
    fn credentials_in_urls_are_percent_decoded() {
        assert_eq!(percent_decode("p%40ss%2Fword+1%"), "p@ss/word+1%");
    }
}
