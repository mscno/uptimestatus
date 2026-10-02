//! What a monitor checks.

use std::{fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{JsonRule, StatusRanges};

/// The kind of check a monitor performs, with its type-specific settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
// One variant is much larger, but specs are few and short-lived.
#[allow(clippy::large_enum_variant)]
pub enum CheckSpec {
    Http(HttpCheck),
    Tcp(TcpCheck),
    Dns(DnsCheck),
    /// The service reports in (`/api/push/{token}`); silence means DOWN.
    Push(PushCheck),
}

impl CheckSpec {
    /// `http`, `tcp`, `dns` or `push`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Tcp(_) => "tcp",
            Self::Dns(_) => "dns",
            Self::Push(_) => "push",
        }
    }
}

/// An HTTP(S) request whose response is evaluated.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpCheck {
    pub url: Url,
    #[serde(default)]
    pub method: HttpMethod,
    /// Extra request headers as `(name, value)` pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Credentials sent with the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<HttpAuth>,
    #[serde(default)]
    pub accepted_status: StatusRanges,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword: Option<KeywordRule>,
    /// A JSON path the response body must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonRule>,
    /// Redirects to follow; 0 means the first response counts.
    #[serde(default = "HttpCheck::default_max_redirects")]
    pub max_redirects: u8,
    #[serde(default)]
    pub ignore_tls_errors: bool,
    /// Warn this many days before the TLS certificate expires (0: never).
    #[serde(default = "HttpCheck::default_cert_warn_days")]
    pub cert_expiry_warn_days: u32,
}

impl fmt::Debug for HttpCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpCheck")
            .field("method", &self.method)
            .field("host", &self.url.host_str())
            .finish_non_exhaustive()
    }
}

impl HttpCheck {
    const fn default_max_redirects() -> u8 {
        10
    }

    const fn default_cert_warn_days() -> u32 {
        14
    }

    /// A plain `GET` with default settings.
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: HttpMethod::default(),
            headers: Vec::new(),
            body: None,
            auth: None,
            accepted_status: StatusRanges::default(),
            keyword: None,
            json: None,
            max_redirects: Self::default_max_redirects(),
            ignore_tls_errors: false,
            cert_expiry_warn_days: Self::default_cert_warn_days(),
        }
    }
}

/// Credentials for an HTTP request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case", deny_unknown_fields)]
pub enum HttpAuth {
    Basic { username: String, password: String },
    Bearer { token: String },
}

// Secrets stay out of logs.
impl fmt::Debug for HttpAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => write!(f, "Basic({username}:***)"),
            Self::Bearer { .. } => f.write_str("Bearer(***)"),
        }
    }
}

/// Response body rule: the text must (or, with `absent`, must not) appear.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeywordRule {
    pub text: String,
    #[serde(default)]
    pub absent: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    #[default]
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
        }
    }
}

/// A DNS lookup; UP when the name resolves (to `expect`, if given).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsCheck {
    pub name: String,
    #[serde(default)]
    pub record_type: DnsRecordType,
    /// Ask this resolver instead of the system's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<IpAddr>,
    /// Text one of the answers must contain (case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

impl fmt::Debug for DnsCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsCheck")
            .field("name", &self.name)
            .field("record_type", &self.record_type)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DnsRecordType {
    #[default]
    A,
    Aaaa,
    Cname,
    Mx,
    Ns,
    Txt,
}

impl DnsRecordType {
    pub const ALL: [Self; 6] = [
        Self::A,
        Self::Aaaa,
        Self::Cname,
        Self::Mx,
        Self::Ns,
        Self::Txt,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Cname => "CNAME",
            Self::Mx => "MX",
            Self::Ns => "NS",
            Self::Txt => "TXT",
        }
    }
}

impl fmt::Display for DnsRecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DnsRecordType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown record type `{s}`"))
    }
}

/// A heartbeat the monitored service sends.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushCheck {
    /// The secret part of the push URL.
    pub token: String,
}

impl fmt::Debug for PushCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushCheck").finish_non_exhaustive()
    }
}

/// A TCP connect check, optionally speaking a little: over TLS, sending a
/// line and expecting text back (Redis `PING`/`+PONG`, an SMTP `220`
/// banner, SSH, MySQL, ...).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpCheck {
    pub host: String,
    pub port: u16,
    /// Sent once connected. `\r`, `\n`, `\t` and `\\` are escapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<String>,
    /// The reply must contain this text (`send` may be empty: wait for a banner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// Wrap the connection in TLS (implicit TLS such as SMTPS/IMAPS/LDAPS).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls: bool,
    /// TLS only: accept certificates that do not verify.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_tls_errors: bool,
    /// TLS only: warn this many days before the certificate expires (0: never).
    #[serde(
        default = "TcpCheck::default_cert_warn_days",
        skip_serializing_if = "TcpCheck::is_default_cert_warn_days"
    )]
    pub cert_expiry_warn_days: u32,
}

impl fmt::Debug for TcpCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpCheck")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl TcpCheck {
    const fn default_cert_warn_days() -> u32 {
        14
    }

    fn is_default_cert_warn_days(days: &u32) -> bool {
        *days == Self::default_cert_warn_days()
    }

    /// A plain connect check.
    pub fn connect(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            send: None,
            expect: None,
            tls: false,
            ignore_tls_errors: false,
            cert_expiry_warn_days: Self::default_cert_warn_days(),
        }
    }

    /// `send` with its escapes resolved.
    pub fn send_bytes(&self) -> Vec<u8> {
        unescape(self.send.as_deref().unwrap_or_default())
    }
}

/// `"PING\\r\\n"` (backslash sequences) as bytes; unknown escapes stay as typed.
pub fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some(other) => {
                out.push(b'\\');
                let mut buffer = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn dns_and_push_checks_round_trip() {
        let dns: CheckSpec = serde_json::from_str(
            r#"{"type":"dns","name":"example.com","record_type":"TXT","expect":"v=spf1"}"#,
        )
        .unwrap();
        assert_eq!(dns.kind(), "dns");
        let CheckSpec::Dns(ref check) = dns else {
            unreachable!()
        };
        assert_eq!(check.record_type, DnsRecordType::Txt);
        assert_eq!(check.resolver, None);
        let push: CheckSpec = serde_json::from_str(r#"{"type":"push","token":"abc"}"#).unwrap();
        assert_eq!(
            push,
            CheckSpec::Push(PushCheck {
                token: "abc".into()
            })
        );
        assert_eq!("aaaa".parse::<DnsRecordType>(), Ok(DnsRecordType::Aaaa));
    }

    #[test]
    fn http_check_uses_sensible_defaults() {
        let spec: CheckSpec =
            serde_json::from_str(r#"{"type":"http","url":"https://example.com/health"}"#).unwrap();
        assert_eq!(
            spec,
            CheckSpec::Http(HttpCheck::get(
                "https://example.com/health".parse().unwrap()
            ))
        );
    }

    #[test]
    fn send_text_resolves_escapes() {
        let check = TcpCheck {
            send: Some("PING\\r\\nA\\\\B\\x".into()),
            ..TcpCheck::connect("h", 1)
        };
        assert_eq!(check.send_bytes(), b"PING\r\nA\\B\\x");
        assert_eq!(TcpCheck::connect("h", 1).send_bytes(), b"");
    }

    #[test]
    fn a_plain_tcp_check_serializes_compactly_and_extends_old_specs() {
        let json = serde_json::to_value(TcpCheck::connect("db", 5432)).unwrap();
        assert_eq!(json, serde_json::json!({"host": "db", "port": 5432}));
        let old: TcpCheck =
            serde_json::from_value(serde_json::json!({"host": "db", "port": 5432})).unwrap();
        assert_eq!(old, TcpCheck::connect("db", 5432));
    }

    #[test]
    fn http_check_round_trips_with_all_options() {
        let spec = CheckSpec::Http(HttpCheck {
            url: "https://example.com/admin".parse().unwrap(),
            method: HttpMethod::Head,
            headers: vec![("x-probe".into(), "1".into())],
            body: None,
            auth: Some(HttpAuth::Bearer {
                token: "t0ken".into(),
            }),
            accepted_status: "403".parse().unwrap(),
            keyword: Some(KeywordRule {
                text: "ok".into(),
                absent: true,
            }),
            json: Some(JsonRule {
                path: "status".into(),
                expect: Some("ok".into()),
            }),
            max_redirects: 0,
            ignore_tls_errors: true,
            cert_expiry_warn_days: 7,
        });
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["method"], "HEAD");
        assert_eq!(json["accepted_status"], "403");
        assert_eq!(json["auth"]["scheme"], "bearer");
        assert_eq!(serde_json::from_value::<CheckSpec>(json).unwrap(), spec);
    }

    #[test]
    fn auth_debug_hides_secrets() {
        let basic = HttpAuth::Basic {
            username: "bob".into(),
            password: "hunter2".into(),
        };
        let shown = format!(
            "{basic:?} {:?}",
            HttpAuth::Bearer {
                token: "abc".into()
            }
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("abc"),
            "{shown}"
        );
    }

    #[test]
    fn check_debug_hides_request_secrets() {
        let mut http = HttpCheck::get(
            "https://user:password@example.com/private?token=secret"
                .parse()
                .unwrap(),
        );
        http.headers
            .push(("X-Custom".into(), "header-secret".into()));
        http.body = Some("body-secret".into());
        let shown = format!("{:?}", CheckSpec::Http(http));
        for secret in [
            "user",
            "password",
            "private",
            "secret",
            "header-secret",
            "body-secret",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(
            !format!(
                "{:?}",
                PushCheck {
                    token: "push-secret".into()
                }
            )
            .contains("push-secret")
        );
        let mut tcp = TcpCheck::connect("example.com", 443);
        tcp.send = Some("tcp-secret".into());
        assert!(!format!("{tcp:?}").contains("tcp-secret"));
    }

    #[test]
    fn tcp_check_round_trips() {
        let spec = CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432));
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(
            json,
            r#"{"type":"tcp","host":"db.example.com","port":5432}"#
        );
        assert_eq!(serde_json::from_str::<CheckSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn rejects_unknown_check_types() {
        assert!(serde_json::from_str::<CheckSpec>(r#"{"type":"ping","host":"x"}"#).is_err());
    }

    #[test]
    fn http_check_rejects_unknown_fields() {
        let json = r#"{"type":"http","url":"https://example.com","acepted_status":"403"}"#;
        let error = serde_json::from_str::<CheckSpec>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("acepted_status"), "{error}");
    }

    #[test]
    fn keyword_rule_rejects_unknown_fields() {
        assert!(serde_json::from_str::<KeywordRule>(r#"{"text":"ok","absnt":true}"#).is_err());
    }
}
