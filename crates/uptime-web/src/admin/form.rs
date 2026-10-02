//! The monitor form: raw browser input ⇄ [`MonitorSpec`], with per-field errors.
//!
//! Every field is kept as the text the user typed, so a form with errors can be
//! re-rendered exactly as submitted.

use std::{collections::BTreeMap, fmt, time::Duration};

use serde::Deserialize;
use uptime_domain::{
    CheckPolicy, CheckSpec, DnsCheck, DnsRecordType, HttpAuth, HttpCheck, HttpMethod, JsonRule,
    KeywordRule, MonitorKey, MonitorSpec, PolicyError, PushCheck, StatusRanges, TcpCheck,
    normalize_group, normalize_tags,
};
use url::Url;

/// Field name → message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormErrors(BTreeMap<&'static str, String>);

impl FormErrors {
    pub fn get(&self, field: &str) -> Option<&str> {
        self.0.get(field).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn insert(&mut self, field: &'static str, message: impl Into<String>) {
        self.0.entry(field).or_insert_with(|| message.into());
    }

    pub fn fields(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.keys().copied()
    }
}

/// `application/x-www-form-urlencoded` body of the monitor form.
///
/// Checkboxes are `Some("on")` when ticked and absent otherwise; browsers omit
/// unticked checkboxes, so missing fields must deserialize as empty (hence the
/// derived `Default`, not the pre-filled [`MonitorForm::new_monitor`]).
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MonitorForm {
    pub key: String,
    pub name: String,
    pub check_type: String,
    pub url: String,
    pub method: String,
    pub headers: String,
    pub body: String,
    pub accepted_status: String,
    pub keyword: String,
    pub keyword_absent: Option<String>,
    pub tags: String,
    pub group: String,
    pub json_path: String,
    pub json_expect: String,
    pub auth_type: String,
    pub auth_username: String,
    pub auth_password: String,
    pub auth_token: String,
    pub max_redirects: String,
    pub ignore_tls_errors: Option<String>,
    pub cert_warn_days: String,
    pub host: String,
    pub port: String,
    pub tcp_send: String,
    pub tcp_expect: String,
    pub tcp_tls: Option<String>,
    pub tcp_ignore_tls_errors: Option<String>,
    pub tcp_cert_warn_days: String,
    pub dns_name: String,
    pub record_type: String,
    pub resolver: String,
    pub expect: String,
    pub push_token: String,
    pub interval: String,
    pub retry_interval: String,
    pub timeout: String,
    pub retries: String,
    pub invert: Option<String>,
    pub degraded_after: String,
    pub resend_every: String,
    pub active: Option<String>,
    /// Alert channel toggles, `channel_<id>=on`, and anything else sent along.
    #[serde(flatten)]
    pub extra: BTreeMap<String, String>,
}

impl fmt::Debug for MonitorForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonitorForm")
            .field("key", &self.key)
            .field("check_type", &self.check_type)
            .finish_non_exhaustive()
    }
}

impl MonitorForm {
    /// A copy of `spec` for the "new monitor" form: the next free key after
    /// `taken`, "(copy)" in the name, and no push token (a new one is made on save).
    pub fn duplicate(spec: &MonitorSpec, taken: &[String]) -> Self {
        let mut form = Self::from_spec(spec);
        form.key = super::copy_of(spec.key.as_str(), taken, MonitorKey::MAX_LEN);
        form.name = format!("{} (copy)", spec.name);
        form.push_token.clear();
        form
    }

    /// Channels switched on in the form (`channel_<id>=on`), by id.
    pub fn channel_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .extra
            .iter()
            .filter(|(_, value)| value.as_str() == "on")
            .filter_map(|(key, _)| key.strip_prefix("channel_")?.parse().ok())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Switches these channels on.
    #[must_use]
    pub fn with_channels(mut self, ids: &[i64]) -> Self {
        for id in ids {
            self.extra.insert(format!("channel_{id}"), "on".to_owned());
        }
        self
    }

    pub fn has_channel(&self, id: i64) -> bool {
        self.extra
            .get(&format!("channel_{id}"))
            .is_some_and(|v| v == "on")
    }

    /// The form for a new monitor: an active HTTP check with the default policy.
    pub fn new_monitor() -> Self {
        let policy = CheckPolicy::default();
        Self {
            check_type: "http".into(),
            method: HttpMethod::Get.as_str().into(),
            accepted_status: StatusRanges::default().to_string(),
            max_redirects: "10".into(),
            cert_warn_days: "14".into(),
            tcp_cert_warn_days: "14".into(),
            interval: exact(policy.interval),
            retry_interval: exact(policy.retry_interval),
            timeout: exact(policy.timeout),
            retries: policy.retries.to_string(),
            resend_every: policy.resend_every.to_string(),
            active: flag(true),
            ..Self::default()
        }
    }

    /// Pre-fills the form from an existing monitor.
    pub fn from_spec(spec: &MonitorSpec) -> Self {
        let policy = &spec.policy;
        let mut form = Self {
            key: spec.key.to_string(),
            name: spec.name.clone(),
            tags: spec.tags.join(", "),
            group: spec.group.clone().unwrap_or_default(),
            interval: exact(policy.interval),
            retry_interval: exact(policy.retry_interval),
            timeout: exact(policy.timeout),
            retries: policy.retries.to_string(),
            invert: flag(policy.invert),
            degraded_after: policy.degraded_after.map(exact).unwrap_or_default(),
            resend_every: policy.resend_every.to_string(),
            active: flag(spec.active),
            ..Self::default()
        };
        match &spec.check {
            CheckSpec::Http(http) => {
                form.check_type = "http".into();
                form.url = http.url.to_string();
                form.method = http.method.as_str().into();
                form.headers = http
                    .headers
                    .iter()
                    .map(|(n, v)| format!("{n}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                form.body = http.body.clone().unwrap_or_default();
                form.accepted_status = http.accepted_status.to_string();
                if let Some(rule) = &http.keyword {
                    form.keyword = rule.text.clone();
                    form.keyword_absent = flag(rule.absent);
                }
                if let Some(rule) = &http.json {
                    form.json_path = rule.path.clone();
                    form.json_expect = rule.expect.clone().unwrap_or_default();
                }
                match &http.auth {
                    Some(HttpAuth::Basic { username, password }) => {
                        form.auth_type = "basic".into();
                        form.auth_username = username.clone();
                        form.auth_password = password.clone();
                    }
                    Some(HttpAuth::Bearer { token }) => {
                        form.auth_type = "bearer".into();
                        form.auth_token = token.clone();
                    }
                    None => {}
                }
                form.max_redirects = http.max_redirects.to_string();
                form.ignore_tls_errors = flag(http.ignore_tls_errors);
                form.cert_warn_days = http.cert_expiry_warn_days.to_string();
            }
            CheckSpec::Tcp(tcp) => {
                form.check_type = "tcp".into();
                form.host = tcp.host.clone();
                form.port = tcp.port.to_string();
                form.tcp_send = tcp.send.clone().unwrap_or_default();
                form.tcp_expect = tcp.expect.clone().unwrap_or_default();
                form.tcp_tls = flag(tcp.tls);
                form.tcp_ignore_tls_errors = flag(tcp.ignore_tls_errors);
                form.tcp_cert_warn_days = tcp.cert_expiry_warn_days.to_string();
            }
            CheckSpec::Dns(dns) => {
                form.check_type = "dns".into();
                form.dns_name = dns.name.clone();
                form.record_type = dns.record_type.as_str().into();
                form.resolver = dns.resolver.map(|ip| ip.to_string()).unwrap_or_default();
                form.expect = dns.expect.clone().unwrap_or_default();
            }
            CheckSpec::Push(push) => {
                form.check_type = "push".into();
                form.push_token = push.token.clone();
            }
        }
        form
    }

    /// Validates the input and builds the spec, or reports every problem found.
    pub fn to_spec(&self) -> Result<MonitorSpec, FormErrors> {
        let mut errors = FormErrors::default();

        let key = match self.key.trim().parse::<MonitorKey>() {
            Ok(key) => Some(key),
            Err(_) => {
                errors.insert(
                    "key",
                    "Use 1-64 lowercase letters, digits and dashes (e.g. api-health).",
                );
                None
            }
        };
        let name = self.name.trim();
        if name.is_empty() {
            errors.insert("name", "Give the monitor a name.");
        } else if name.chars().count() > 100 {
            errors.insert("name", "Keep the name under 100 characters.");
        }

        let check = match self.check_type.trim() {
            "http" | "" => self.http_check(&mut errors).map(CheckSpec::Http),
            "tcp" => self.tcp_check(&mut errors).map(CheckSpec::Tcp),
            "dns" => self.dns_check(&mut errors).map(CheckSpec::Dns),
            "push" => Some(CheckSpec::Push(PushCheck {
                token: Some(self.push_token.trim().to_owned())
                    .filter(|t| t.len() >= 16 && t.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or_else(new_push_token),
            })),
            _ => {
                errors.insert("check_type", "Choose HTTP, TCP, DNS or push.");
                None
            }
        };

        let interval = required_duration(&mut errors, "interval", &self.interval);
        let retry_interval = match blank(&self.retry_interval) {
            None => interval,
            Some(text) => duration(&mut errors, "retry_interval", text),
        };
        let timeout = match blank(&self.timeout) {
            None => Some(CheckPolicy::default().timeout),
            Some(text) => duration(&mut errors, "timeout", text),
        };
        let degraded_after =
            blank(&self.degraded_after).map(|text| duration(&mut errors, "degraded_after", text));
        let retries = count(&mut errors, "retries", &self.retries, 0, 20);
        let resend_every = count(&mut errors, "resend_every", &self.resend_every, 0, 1000);
        let tags = normalize_tags(&self.tags)
            .map_err(|e| errors.insert("tags", sentence(&e)))
            .unwrap_or_default();
        let group = normalize_group(&self.group)
            .map_err(|e| errors.insert("group", sentence(&e)))
            .unwrap_or_default();

        if !errors.is_empty() {
            return Err(errors);
        }
        let (
            Some(key),
            Some(check),
            Some(interval),
            Some(retry_interval),
            Some(timeout),
            Some(retries),
            Some(resend_every),
        ) = (
            key,
            check,
            interval,
            retry_interval,
            timeout,
            retries,
            resend_every,
        )
        else {
            errors.insert("form", "The form is incomplete.");
            return Err(errors);
        };
        let policy = CheckPolicy {
            interval,
            retry_interval,
            timeout,
            retries,
            invert: self.invert.is_some(),
            degraded_after: degraded_after.flatten(),
            resend_every,
        };
        if let Err(error) = policy.validate() {
            errors.insert(policy_field(&error), sentence(&error));
            return Err(errors);
        }
        Ok(MonitorSpec {
            key,
            name: name.to_owned(),
            check,
            policy,
            active: self.active.is_some(),
            tags,
            group,
        })
    }

    fn http_check(&self, errors: &mut FormErrors) -> Option<HttpCheck> {
        let url = match self.url.trim().parse::<Url>() {
            Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
                Some(url)
            }
            _ => {
                errors.insert("url", "Enter a full http:// or https:// URL.");
                None
            }
        };
        let method = match blank(&self.method) {
            None => Some(HttpMethod::Get),
            Some(text) => {
                let method = serde_json::from_value::<HttpMethod>(serde_json::Value::String(
                    text.to_ascii_uppercase(),
                ))
                .ok();
                if method.is_none() {
                    errors.insert(
                        "method",
                        "Choose GET, HEAD, POST, PUT, PATCH, DELETE or OPTIONS.",
                    );
                }
                method
            }
        };
        let headers = self
            .headers
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .try_fold(Vec::new(), |mut headers, line| {
                let (name, value) = line.split_once(':')?;
                let name = name.trim();
                let valid = !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b));
                valid.then(|| {
                    headers.push((name.to_owned(), value.trim().to_owned()));
                    headers
                })
            });
        if headers.is_none() {
            errors.insert(
                "headers",
                "Write one header per line, like `Authorization: Bearer …`.",
            );
        }
        let accepted_status = match blank(&self.accepted_status) {
            None => Some(StatusRanges::default()),
            Some(text) => text
                .parse::<StatusRanges>()
                .map_err(|e| errors.insert("accepted_status", sentence(&e)))
                .ok(),
        };
        let max_redirects = count(errors, "max_redirects", &self.max_redirects, 10, 20)
            .and_then(|n| u8::try_from(n).ok());
        let cert_warn_days = count(errors, "cert_warn_days", &self.cert_warn_days, 14, 365);
        let keyword = blank(&self.keyword).map(|text| KeywordRule {
            text: text.to_owned(),
            absent: self.keyword_absent.is_some(),
        });

        let json = blank(&self.json_path).map(|path| JsonRule {
            path: path.to_owned(),
            expect: blank(&self.json_expect).map(str::to_owned),
        });
        if let Some(rule) = &json
            && let Err(error) = rule.validate()
        {
            errors.insert("json_path", sentence(&error));
        }
        let auth = match self.auth_type.trim() {
            "basic" => {
                if self.auth_username.trim().is_empty() {
                    errors.insert("auth_username", "Enter the username.");
                }
                Some(HttpAuth::Basic {
                    username: self.auth_username.trim().to_owned(),
                    password: self.auth_password.clone(),
                })
            }
            "bearer" => {
                if self.auth_token.trim().is_empty() {
                    errors.insert("auth_token", "Enter the token.");
                }
                Some(HttpAuth::Bearer {
                    token: self.auth_token.trim().to_owned(),
                })
            }
            _ => None,
        };

        Some(HttpCheck {
            url: url?,
            auth,
            json: json.filter(|rule| rule.validate().is_ok()),
            method: method?,
            headers: headers?,
            body: blank(&self.body).map(str::to_owned),
            accepted_status: accepted_status?,
            keyword,
            max_redirects: max_redirects?,
            ignore_tls_errors: self.ignore_tls_errors.is_some(),
            cert_expiry_warn_days: cert_warn_days?,
        })
    }

    fn tcp_check(&self, errors: &mut FormErrors) -> Option<TcpCheck> {
        let host = self.host.trim();
        let host_ok =
            !host.is_empty() && !host.contains(char::is_whitespace) && !host.contains("://");
        if !host_ok {
            errors.insert("host", "Enter a hostname or IP address, without a scheme.");
        }
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0);
        if port.is_none() {
            errors.insert("port", "Enter a port between 1 and 65535.");
        }
        let cert_warn_days = count(
            errors,
            "tcp_cert_warn_days",
            &self.tcp_cert_warn_days,
            14,
            365,
        );
        Some(TcpCheck {
            send: (!self.tcp_send.is_empty()).then(|| self.tcp_send.clone()),
            expect: (!self.tcp_expect.is_empty()).then(|| self.tcp_expect.clone()),
            tls: self.tcp_tls.is_some(),
            ignore_tls_errors: self.tcp_ignore_tls_errors.is_some(),
            cert_expiry_warn_days: cert_warn_days?,
            ..TcpCheck::connect(host_ok.then(|| host.to_owned())?, port?)
        })
    }
}

impl MonitorForm {
    fn dns_check(&self, errors: &mut FormErrors) -> Option<DnsCheck> {
        let name = self.dns_name.trim().trim_end_matches('.');
        let name_ok =
            name.contains('.') && !name.contains(char::is_whitespace) && !name.contains("://");
        if !name_ok {
            errors.insert("dns_name", "Enter a domain name, e.g. example.com.");
        }
        let record_type = match blank(&self.record_type) {
            None => Some(DnsRecordType::A),
            Some(text) => text
                .parse()
                .map_err(|e: String| errors.insert("record_type", e))
                .ok(),
        };
        let resolver = match blank(&self.resolver) {
            None => Some(None),
            Some(text) => match text.parse::<std::net::IpAddr>() {
                Ok(ip) => Some(Some(ip)),
                Err(_) => {
                    errors.insert(
                        "resolver",
                        "Enter an IP address, e.g. 1.1.1.1, or leave blank.",
                    );
                    None
                }
            },
        };
        Some(DnsCheck {
            name: name_ok.then(|| name.to_owned())?,
            record_type: record_type?,
            resolver: resolver?,
            expect: blank(&self.expect).map(str::to_owned),
        })
    }
}

/// A fresh push token: 24 random letters and digits.
fn new_push_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        // Practically unreachable; fall back to the clock rather than fail the form.
        let nanos = jiff::Timestamp::now().as_nanosecond();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (nanos >> (i % 16 * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect()
}

fn flag(on: bool) -> Option<String> {
    on.then(|| "on".to_owned())
}

/// Exact, re-parseable duration text ("1m", "1m 30s", "1500ms").
fn exact(duration: Duration) -> String {
    humantime::format_duration(duration).to_string()
}

fn blank(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

/// `30s`, `1m 30s`, `800ms` (humantime), or a decimal with one unit: `1.5s`,
/// `0.5m`, `2.5ms`.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if let Ok(duration) = humantime::parse_duration(text) {
        return Some(duration);
    }
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let seconds = match unit.trim() {
        "ms" => value / 1000.0,
        "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value * 60.0,
        _ => return None,
    };
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn duration(errors: &mut FormErrors, field: &'static str, text: &str) -> Option<Duration> {
    let parsed = parse_duration(text);
    if parsed.is_none() {
        let example = if field == "degraded_after" {
            "Use a duration like 800ms, 1.5s or 2s."
        } else {
            "Use a duration like 30s, 1m or 1m 30s."
        };
        errors.insert(field, example);
    }
    parsed
}

fn required_duration(errors: &mut FormErrors, field: &'static str, text: &str) -> Option<Duration> {
    match blank(text) {
        Some(text) => duration(errors, field, text),
        None => {
            errors.insert(field, "Required.");
            None
        }
    }
}

fn count(
    errors: &mut FormErrors,
    field: &'static str,
    text: &str,
    default: u32,
    max: u32,
) -> Option<u32> {
    let Some(text) = blank(text) else {
        return Some(default);
    };
    let parsed = text.parse::<u32>().ok().filter(|n| *n <= max);
    if parsed.is_none() {
        errors.insert(field, format!("Enter a whole number from 0 to {max}."));
    }
    parsed
}

/// An error message as a sentence: capitalized, ending with a period.
fn sentence(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    let capitalized = chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect::<String>())
        .unwrap_or_default();
    if capitalized.ends_with('.') {
        capitalized
    } else {
        format!("{capitalized}.")
    }
}

/// The form field a policy rule belongs to.
fn policy_field(error: &PolicyError) -> &'static str {
    match error {
        PolicyError::Interval { .. } => "interval",
        PolicyError::RetryInterval { .. } => "retry_interval",
        PolicyError::Timeout { .. } => "timeout",
        PolicyError::DegradedAfter => "degraded_after",
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn copies_of_long_keys_stay_valid() {
        let base = "a".repeat(64);
        let copy = crate::admin::copy_of(&base, &[], MonitorKey::MAX_LEN);
        assert_eq!(copy.len(), 64);
        assert!(copy.ends_with("-copy"));
        assert!(copy.parse::<MonitorKey>().is_ok());
        let taken = vec![copy.clone()];
        let second = crate::admin::copy_of(&base, &taken, MonitorKey::MAX_LEN);
        assert!(
            second.ends_with("-copy-2") && second.len() <= 64,
            "{second}"
        );
    }

    #[rstest::rstest]
    #[case("800ms", 800)]
    #[case("1.5s", 1500)]
    #[case("2s", 2000)]
    #[case("1s 500ms", 1500)]
    #[case(" 0.5m ", 30_000)]
    #[case("2.5ms", 2)]
    fn durations_accept_decimals(#[case] text: &str, #[case] millis: u128) {
        assert_eq!(parse_duration(text).map(|d| d.as_millis()), Some(millis));
    }

    #[rstest::rstest]
    #[case("fast")]
    #[case("800")]
    #[case("1.5h?")]
    #[case("")]
    fn durations_reject_nonsense(#[case] text: &str) {
        assert_eq!(parse_duration(text), None);
    }

    fn http_form() -> MonitorForm {
        MonitorForm {
            key: "api".into(),
            name: "API".into(),
            url: "https://api.example.com/health".into(),
            ..MonitorForm::new_monitor()
        }
    }

    fn errors(form: &MonitorForm) -> Vec<&'static str> {
        form.to_spec().unwrap_err().fields().collect()
    }

    #[test]
    fn new_monitor_defaults_describe_an_active_http_monitor() {
        let form = MonitorForm::new_monitor();
        assert_eq!(form.check_type, "http");
        assert_eq!(form.method, "GET");
        assert_eq!(form.accepted_status, "200-299");
        assert_eq!(form.interval, "1m");
        assert_eq!(form.timeout, "10s");
        assert_eq!(form.active.as_deref(), Some("on"));
    }

    #[test]
    fn builds_an_http_spec_with_defaults() {
        let spec = http_form().to_spec().unwrap();
        assert_eq!(spec.key.as_str(), "api");
        assert_eq!(spec.name, "API");
        assert_eq!(
            spec.check,
            CheckSpec::Http(HttpCheck::get(
                "https://api.example.com/health".parse().unwrap()
            ))
        );
        assert_eq!(spec.policy, CheckPolicy::default());
        assert!(spec.active);
    }

    #[test]
    fn tcp_checks_can_talk_and_use_tls() {
        let form = MonitorForm {
            check_type: "tcp".into(),
            host: "cache.example.com".into(),
            port: "6379".into(),
            tcp_send: "PING\\r\\n".into(),
            tcp_expect: "+PONG".into(),
            tcp_tls: Some("on".into()),
            tcp_cert_warn_days: "30".into(),
            url: String::new(),
            ..http_form()
        };

        let spec = form.to_spec().unwrap();

        let CheckSpec::Tcp(tcp) = &spec.check else {
            panic!("tcp")
        };
        assert_eq!(tcp.send.as_deref(), Some("PING\\r\\n"));
        assert_eq!(tcp.expect.as_deref(), Some("+PONG"));
        assert!(tcp.tls && !tcp.ignore_tls_errors);
        assert_eq!(tcp.cert_expiry_warn_days, 30);
        assert_eq!(MonitorForm::from_spec(&spec).to_spec().unwrap(), spec);
    }

    #[test]
    fn groups_are_normalized_and_validated() {
        let form = MonitorForm {
            group: " Prod / EU ".into(),
            ..http_form()
        };
        let spec = form.to_spec().unwrap();
        assert_eq!(spec.group.as_deref(), Some("prod/eu"));
        assert_eq!(MonitorForm::from_spec(&spec).group, "prod/eu");
        let bad = MonitorForm {
            group: "a//b".into(),
            ..http_form()
        };
        assert_eq!(errors(&bad), ["group"]);
        assert_eq!(http_form().to_spec().unwrap().group, None);
    }

    #[test]
    fn tags_are_normalized_and_validated() {
        let form = MonitorForm {
            tags: "Prod, api prod".into(),
            ..http_form()
        };
        assert_eq!(form.to_spec().unwrap().tags, ["api", "prod"]);
        assert_eq!(
            MonitorForm::from_spec(&form.to_spec().unwrap()).tags,
            "api, prod"
        );
        let bad = MonitorForm {
            tags: "no way!".into(),
            ..http_form()
        };
        assert_eq!(errors(&bad), ["tags"]);
    }

    #[test]
    fn builds_auth_and_json_rules_and_round_trips_them() {
        let form = MonitorForm {
            auth_type: "basic".into(),
            auth_username: "bob".into(),
            auth_password: "hunter2".into(),
            json_path: "data.status".into(),
            json_expect: "ok".into(),
            ..http_form()
        };

        let spec = form.to_spec().unwrap();

        let CheckSpec::Http(http) = &spec.check else {
            panic!("http")
        };
        assert_eq!(
            http.auth,
            Some(HttpAuth::Basic {
                username: "bob".into(),
                password: "hunter2".into()
            })
        );
        assert_eq!(
            http.json,
            Some(JsonRule {
                path: "data.status".into(),
                expect: Some("ok".into())
            })
        );
        let again = MonitorForm::from_spec(&spec);
        assert_eq!(
            (again.auth_type.as_str(), again.json_path.as_str()),
            ("basic", "data.status")
        );
        assert_eq!(again.to_spec().unwrap(), spec);
    }

    #[test]
    fn rejects_bad_json_paths_and_incomplete_auth() {
        let form = MonitorForm {
            auth_type: "bearer".into(),
            json_path: "a[x]".into(),
            ..http_form()
        };
        assert_eq!(errors(&form), ["auth_token", "json_path"]);
    }

    #[test]
    fn builds_an_http_spec_with_every_option() {
        let form = MonitorForm {
            method: "POST".into(),
            headers: "X-Probe: 1\n\nAuthorization: Bearer abc:def\n".into(),
            body: "ping".into(),
            accepted_status: "401, 403".into(),
            keyword: "ok".into(),
            keyword_absent: Some("on".into()),
            max_redirects: "0".into(),
            ignore_tls_errors: Some("on".into()),
            interval: "30s".into(),
            retry_interval: "10s".into(),
            timeout: "5s".into(),
            retries: "2".into(),
            invert: Some("on".into()),
            degraded_after: "1500ms".into(),
            resend_every: "3".into(),
            active: None,
            ..http_form()
        };

        let spec = form.to_spec().unwrap();

        let CheckSpec::Http(http) = &spec.check else {
            panic!("http")
        };
        assert_eq!(http.method, HttpMethod::Post);
        assert_eq!(
            http.headers,
            [
                ("X-Probe".to_owned(), "1".to_owned()),
                ("Authorization".to_owned(), "Bearer abc:def".to_owned())
            ]
        );
        assert_eq!(http.body.as_deref(), Some("ping"));
        assert!(http.accepted_status.contains(403) && !http.accepted_status.contains(200));
        assert_eq!(
            http.keyword,
            Some(KeywordRule {
                text: "ok".into(),
                absent: true
            })
        );
        assert_eq!(http.max_redirects, 0);
        assert!(http.ignore_tls_errors);
        assert_eq!(
            spec.policy,
            CheckPolicy {
                interval: Duration::from_secs(30),
                retry_interval: Duration::from_secs(10),
                timeout: Duration::from_secs(5),
                retries: 2,
                invert: true,
                degraded_after: Some(Duration::from_millis(1500)),
                resend_every: 3,
            }
        );
        assert!(!spec.active, "unticked checkbox means inactive");
    }

    #[test]
    fn builds_a_tcp_spec() {
        let form = MonitorForm {
            check_type: "tcp".into(),
            host: "db.example.com".into(),
            port: "5432".into(),
            url: String::new(),
            ..http_form()
        };
        assert_eq!(
            form.to_spec().unwrap().check,
            CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432))
        );
    }

    #[test]
    fn reports_every_invalid_field_at_once() {
        let form = MonitorForm {
            key: "Not A Key".into(),
            name: "  ".into(),
            url: "ftp://nope".into(),
            accepted_status: "abc".into(),
            interval: "soon".into(),
            ..http_form()
        };
        assert_eq!(
            errors(&form),
            ["accepted_status", "interval", "key", "name", "url"]
        );
    }

    #[test]
    fn rejects_malformed_headers_and_methods() {
        let form = MonitorForm {
            headers: "no colon here".into(),
            method: "BREW".into(),
            ..http_form()
        };
        assert_eq!(errors(&form), ["headers", "method"]);
    }

    #[test]
    fn tcp_needs_host_and_valid_port() {
        let form = MonitorForm {
            check_type: "tcp".into(),
            host: String::new(),
            port: "70000".into(),
            ..http_form()
        };
        assert_eq!(errors(&form), ["host", "port"]);
    }

    #[test]
    fn unknown_check_types_are_rejected() {
        let form = MonitorForm {
            check_type: "ping".into(),
            ..http_form()
        };
        assert_eq!(errors(&form), ["check_type"]);
    }

    #[test]
    fn policy_rules_are_reported_on_the_right_field() {
        let too_fast = MonitorForm {
            interval: "5s".into(),
            ..http_form()
        };
        let long_timeout = MonitorForm {
            timeout: "59s".into(),
            ..http_form()
        };
        let slow_threshold = MonitorForm {
            degraded_after: "20s".into(),
            ..http_form()
        };
        assert_eq!(errors(&too_fast), ["interval"]);
        assert_eq!(errors(&long_timeout), ["timeout"]);
        assert_eq!(errors(&slow_threshold), ["degraded_after"]);
    }

    #[test]
    fn round_trips_through_from_spec() {
        let form = MonitorForm {
            headers: "X-Probe: 1".into(),
            keyword: "ok".into(),
            interval: "30s".into(),
            degraded_after: "2s".into(),
            ..http_form()
        };
        let spec = form.to_spec().unwrap();
        assert_eq!(MonitorForm::from_spec(&spec).to_spec().unwrap(), spec);

        let tcp = MonitorForm {
            check_type: "tcp".into(),
            host: "h".into(),
            port: "1".into(),
            ..http_form()
        };
        let tcp_spec = tcp.to_spec().unwrap();
        assert_eq!(
            MonitorForm::from_spec(&tcp_spec).to_spec().unwrap(),
            tcp_spec
        );
    }

    #[test]
    fn missing_optional_inputs_fall_back_to_defaults() {
        let form = MonitorForm {
            key: "api".into(),
            name: "API".into(),
            check_type: "http".into(),
            url: "https://example.com".into(),
            interval: "30s".into(),
            ..MonitorForm::default()
        };
        let spec = form.to_spec().unwrap();
        let CheckSpec::Http(http) = &spec.check else {
            panic!("http")
        };
        assert_eq!(http.method, HttpMethod::Get);
        assert!(http.accepted_status.contains(200));
        assert_eq!(http.max_redirects, 10);
        assert_eq!(
            spec.policy.retry_interval,
            Duration::from_secs(30),
            "defaults to the interval"
        );
        assert_eq!(spec.policy.timeout, Duration::from_secs(10));
        assert_eq!((spec.policy.retries, spec.policy.resend_every), (0, 0));
        assert!(!spec.active, "missing checkbox means unticked");
    }

    #[test]
    fn deserializes_from_a_urlencoded_body() {
        let form: MonitorForm = serde_urlencoded::from_str(
            "key=api&name=API&check_type=http&url=https%3A%2F%2Fexample.com&interval=1m&active=on",
        )
        .unwrap();
        assert_eq!(form.url, "https://example.com");
        assert_eq!(form.active.as_deref(), Some("on"));
        assert_eq!(form.invert, None);
        assert!(form.to_spec().is_ok(), "{:?}", form.to_spec());
    }
}
