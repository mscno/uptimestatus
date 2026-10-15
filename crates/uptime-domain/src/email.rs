//! Email addresses and mailboxes (`Name <address>`), as accepted from forms
//! and configuration.
//!
//! Validation is deliberately practical rather than RFC 5322-complete: one
//! `@`, a local part without spaces or angle brackets, and a dotted domain.
//! Domains are lowercased; local parts keep their case (some servers care).

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// Longest address accepted (RFC 5321's path limit).
const MAX_LEN: usize = 254;
const MAX_LOCAL_LEN: usize = 64;

/// Why a string is not an email address.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a valid email address")]
pub struct EmailError(pub String);

/// A validated email address.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EmailAddress(String);

impl EmailAddress {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part after `@`.
    pub fn domain(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(_, domain)| domain)
    }

    /// The form used to compare and look up addresses: lowercased.
    pub fn normalized(&self) -> String {
        self.0.to_lowercase()
    }

    /// For lists and logs: `al***@example.com`.
    pub fn masked(&self) -> String {
        let (local, domain) = self.0.rsplit_once('@').unwrap_or((&self.0, ""));
        let shown: String = local.chars().take(2).collect();
        format!("{shown}***@{domain}")
    }
}

impl FromStr for EmailAddress {
    type Err = EmailError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let invalid = || EmailError(text.to_owned());
        if text.len() > MAX_LEN {
            return Err(invalid());
        }
        let (local, domain) = text.rsplit_once('@').ok_or_else(invalid)?;
        let local_ok = !local.is_empty()
            && local.len() <= MAX_LOCAL_LEN
            && !local.starts_with('.')
            && !local.ends_with('.')
            && local
                .chars()
                .all(|c| !c.is_whitespace() && !c.is_control() && !"<>()[]\\,;:@\"".contains(c));
        let domain = domain.to_lowercase();
        let labels: Vec<&str> = domain.split('.').collect();
        let domain_ok = labels.len() >= 2
            && labels.iter().all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_alphanumeric() || c == '-')
            });
        if !(local_ok && domain_ok) {
            return Err(invalid());
        }
        Ok(Self(format!("{local}@{domain}")))
    }
}

impl TryFrom<String> for EmailAddress {
    type Error = EmailError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<EmailAddress> for String {
    fn from(address: EmailAddress) -> Self {
        address.0
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An address with an optional display name: `Acme Status <status@acme.com>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Mailbox {
    pub name: Option<String>,
    pub address: EmailAddress,
}

impl Mailbox {
    pub fn new(name: Option<&str>, address: EmailAddress) -> Self {
        let name = name
            .map(|n| n.trim().replace(['"', '\r', '\n', '<', '>'], ""))
            .filter(|n| !n.is_empty());
        Self { name, address }
    }

    /// The same mailbox under another display name.
    #[must_use]
    pub fn named(&self, name: &str) -> Self {
        Self::new(Some(name), self.address.clone())
    }
}

impl FromStr for Mailbox {
    type Err = EmailError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        match (text.rfind('<'), text.strip_suffix('>')) {
            (Some(open), Some(inner)) => {
                let name = text[..open].trim().trim_matches('"').trim();
                let address = inner[open + 1..].parse()?;
                Ok(Self::new((!name.is_empty()).then_some(name), address))
            }
            _ => Ok(Self::new(None, text.parse()?)),
        }
    }
}

impl TryFrom<String> for Mailbox {
    type Error = EmailError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Mailbox> for String {
    fn from(mailbox: Mailbox) -> Self {
        mailbox.to_string()
    }
}

impl fmt::Display for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "\"{name}\" <{}>", self.address),
            None => write!(f, "{}", self.address),
        }
    }
}

/// A page's sending domain as registered at the email provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderDomain {
    /// The provider's id.
    pub id: String,
    pub name: String,
    pub status: DomainState,
    pub records: Vec<DnsRecord>,
    pub checked_at: Option<jiff::Timestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainState {
    Pending,
    Verified,
    Failed,
}

impl DomainState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Waiting for DNS",
            Self::Verified => "Verified",
            Self::Failed => "Verification failed",
        }
    }
}

impl FromStr for DomainState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "verified" => Ok(Self::Verified),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown domain state `{other}`")),
        }
    }
}

/// One DNS record to publish for the sending domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    /// `CNAME`, `TXT` or `MX`.
    pub kind: String,
    /// The full name, e.g. `lm1._domainkey.mail.example.com`.
    pub name: String,
    pub value: String,
    /// What it is for, e.g. `dkim_primary`, `return_path`, `dmarc`.
    pub purpose: String,
    /// `active`, `pending` or `failed`.
    pub status: String,
    pub required: bool,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("ops@example.com", "ops@example.com")]
    #[case("  Ops.Team+alerts@Example.COM ", "Ops.Team+alerts@example.com")]
    #[case("a@mail.privatenpm.com", "a@mail.privatenpm.com")]
    #[case("o'brien@example.co.uk", "o'brien@example.co.uk")]
    fn accepts_addresses(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(input.parse::<EmailAddress>().unwrap().as_str(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("ops")]
    #[case("@example.com")]
    #[case("ops@")]
    #[case("ops@localhost")]
    #[case("ops@exa mple.com")]
    #[case("o ps@example.com")]
    #[case("<ops@example.com>")]
    #[case("ops@example.com\r\nBcc: x@y.com")]
    #[case("ops@-example.com")]
    #[case(".ops@example.com")]
    #[case("ops@example..com")]
    fn rejects_non_addresses(#[case] input: &str) {
        assert!(input.parse::<EmailAddress>().is_err(), "{input}");
    }

    #[test]
    fn rejects_overlong_addresses() {
        let long = format!("{}@example.com", "a".repeat(65));
        assert!(long.parse::<EmailAddress>().is_err());
    }

    #[test]
    fn addresses_mask_and_normalize() {
        let address: EmailAddress = "Alice.Smith@Example.com".parse().unwrap();
        assert_eq!(address.masked(), "Al***@example.com");
        assert_eq!(address.normalized(), "alice.smith@example.com");
        assert_eq!(address.domain(), "example.com");
    }

    #[rstest]
    #[case("status@acme.com", None, "status@acme.com")]
    #[case(
        "Acme Status <status@acme.com>",
        Some("Acme Status"),
        "status@acme.com"
    )]
    #[case(
        "\"Acme, Inc.\" <status@acme.com>",
        Some("Acme, Inc."),
        "status@acme.com"
    )]
    fn mailboxes_parse(#[case] input: &str, #[case] name: Option<&str>, #[case] address: &str) {
        let mailbox: Mailbox = input.parse().unwrap();
        assert_eq!(mailbox.name.as_deref(), name);
        assert_eq!(mailbox.address.as_str(), address);
    }

    #[test]
    fn mailbox_names_cannot_inject_headers() {
        let mailbox = Mailbox::new(
            Some("Evil\r\nBcc: x@y.com <\"a\">"),
            "a@example.com".parse().unwrap(),
        );
        let shown = mailbox.to_string();
        assert!(!shown.contains('\n') && !shown.contains('\r'), "{shown}");
        assert_eq!(shown, "\"EvilBcc: x@y.com a\" <a@example.com>");
        assert_eq!(
            shown.parse::<Mailbox>().unwrap().address.as_str(),
            "a@example.com"
        );
    }
}
