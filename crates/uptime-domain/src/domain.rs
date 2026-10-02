//! Custom domains for status pages: hostname rules and DNS verification.
//!
//! A domain is ours once its DNS points at the edge: a CNAME chain reaching
//! `EDGE_HOST`, or A/AAAA records that are all edge addresses (apex domains).

use std::{fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize};

/// A lowercase, ASCII (punycode) hostname a status page is served on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hostname(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a valid hostname (e.g. status.example.com)")]
pub struct HostnameError(pub String);

impl Hostname {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Hostname {
    type Err = HostnameError;

    /// Accepts `Status.Example.com`, `status.example.com.` and IDNs
    /// (`status.bücher.de` → `status.xn--bcher-kva.de`). Rejects IP addresses,
    /// ports, paths, single labels and anything else that is not a DNS name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = || HostnameError(s.trim().to_owned());
        let trimmed = s.trim();
        let name = trimmed.strip_suffix('.').unwrap_or(trimmed);
        if name.is_empty() || name.contains(|c: char| c == ':' || c == '/' || c.is_whitespace()) {
            return Err(error());
        }
        let ascii = match url::Host::parse(name) {
            Ok(url::Host::Domain(domain)) => domain.to_ascii_lowercase(),
            _ => return Err(error()),
        };
        let labels: Vec<&str> = ascii.split('.').collect();
        let valid_label = |label: &&str| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        };
        if ascii.len() > 253 || labels.len() < 2 || !labels.iter().all(valid_label) {
            return Err(error());
        }
        Ok(Self(ascii))
    }
}

impl TryFrom<String> for Hostname {
    type Error = HostnameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Hostname> for String {
    fn from(value: Hostname) -> Self {
        value.0
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What DNS says about a hostname.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DnsAnswers {
    /// CNAME targets followed from the hostname, in order.
    pub cname_chain: Vec<String>,
    /// Final A/AAAA addresses.
    pub addresses: Vec<IpAddr>,
}

/// Where custom domains must point: a CNAME to `host` (or any name under
/// it), or A/AAAA records for `addresses`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub host: String,
    pub addresses: Vec<IpAddr>,
}

/// Whether `answers` show the domain pointing at the edge; `Err` explains what
/// to fix.
pub fn points_at_edge(host: &Hostname, answers: &DnsAnswers, edge: &Edge) -> Result<(), String> {
    let normalize = |name: &str| name.trim_end_matches('.').to_ascii_lowercase();
    let edge_host = normalize(&edge.host);
    // The edge itself, or a name under it (some platforms give each certificate its own
    // target, e.g. `abc123.<app>.example.net`).
    let under_edge = |target: &String| {
        let target = normalize(target);
        target == edge_host || target.ends_with(&format!(".{edge_host}"))
    };
    if answers.cname_chain.iter().any(under_edge) {
        return Ok(());
    }
    if answers.cname_chain.is_empty() && answers.addresses.is_empty() {
        return Err(format!(
            "{host} does not resolve yet. Add a CNAME record pointing to {edge_host}."
        ));
    }
    if let Some(target) = answers.cname_chain.last() {
        return Err(format!(
            "{host} is a CNAME to {}; point it to {edge_host} instead.",
            normalize(target)
        ));
    }
    if !edge.addresses.is_empty()
        && answers
            .addresses
            .iter()
            .all(|address| edge.addresses.contains(address))
    {
        return Ok(());
    }
    let stray: Vec<String> = answers
        .addresses
        .iter()
        .filter(|a| !edge.addresses.contains(a))
        .map(ToString::to_string)
        .collect();
    Err(format!(
        "{host} resolves to {}; add a CNAME to {edge_host} (or A/AAAA records for the edge addresses).",
        stray.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    fn edge() -> Edge {
        Edge {
            host: "edge.example.com".into(),
            addresses: vec![
                "203.0.113.10".parse().unwrap(),
                "2001:db8::10".parse().unwrap(),
            ],
        }
    }

    fn host(name: &str) -> Hostname {
        name.parse().unwrap()
    }

    #[rstest]
    #[case("status.team.dev", "status.team.dev")]
    #[case("Status.Team.DEV", "status.team.dev")]
    #[case("status.team.dev.", "status.team.dev")]
    #[case("  status.team.dev ", "status.team.dev")]
    #[case("status.bücher.de", "status.xn--bcher-kva.de")]
    #[case("a-b.c-d.io", "a-b.c-d.io")]
    fn accepts_hostnames(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(input.parse::<Hostname>().unwrap().as_str(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("localhost")]
    #[case("192.168.1.1")]
    #[case("status.team.dev:443")]
    #[case("https://status.team.dev")]
    #[case("status.team.dev/path")]
    #[case("-bad.team.dev")]
    #[case("bad-.team.dev")]
    #[case("under_score.team.dev")]
    #[case("spa ce.team.dev")]
    fn rejects_non_hostnames(#[case] input: &str) {
        assert!(input.parse::<Hostname>().is_err(), "{input}");
    }

    #[test]
    fn rejects_overlong_labels() {
        assert!(
            format!("{}.dev", "a".repeat(64))
                .parse::<Hostname>()
                .is_err()
        );
        assert!(
            format!("{}.dev", "a".repeat(63))
                .parse::<Hostname>()
                .is_ok()
        );
    }

    #[test]
    fn a_cname_to_the_edge_verifies() {
        let answers = DnsAnswers {
            cname_chain: vec!["edge.example.com.".into()],
            addresses: vec!["203.0.113.10".parse().unwrap()],
        };
        assert_eq!(
            points_at_edge(&host("status.team.dev"), &answers, &edge()),
            Ok(())
        );
    }

    #[test]
    fn a_cname_to_a_name_under_the_edge_verifies() {
        // Some platforms hand out a per-certificate target such as `abc123.<app>.example.net`.
        let answers = DnsAnswers {
            cname_chain: vec!["pewzr6k.edge.example.com.".into()],
            addresses: vec!["203.0.113.10".parse().unwrap()],
        };
        assert_eq!(
            points_at_edge(&host("status.team.dev"), &answers, &edge()),
            Ok(())
        );
    }

    #[test]
    fn a_lookalike_suffix_does_not_verify() {
        let answers = DnsAnswers {
            cname_chain: vec!["evil-edge.example.com".into()],
            addresses: vec![],
        };
        assert!(points_at_edge(&host("status.team.dev"), &answers, &edge()).is_err());
    }

    #[test]
    fn a_cname_chain_through_other_names_verifies() {
        let answers = DnsAnswers {
            cname_chain: vec!["status.alias.dev".into(), "Edge.Example.com".into()],
            addresses: vec![],
        };
        assert_eq!(
            points_at_edge(&host("status.team.dev"), &answers, &edge()),
            Ok(())
        );
    }

    #[test]
    fn apex_records_on_edge_addresses_verify() {
        let answers = DnsAnswers {
            cname_chain: vec![],
            addresses: edge().addresses,
        };
        assert_eq!(points_at_edge(&host("team.dev"), &answers, &edge()), Ok(()));
    }

    #[test]
    fn stray_addresses_fail_with_a_hint() {
        let answers = DnsAnswers {
            cname_chain: vec![],
            addresses: vec![
                "203.0.113.10".parse().unwrap(),
                "198.51.100.7".parse().unwrap(),
            ],
        };
        let error = points_at_edge(&host("status.team.dev"), &answers, &edge()).unwrap_err();
        assert!(
            error.contains("198.51.100.7") && error.contains("edge.example.com"),
            "{error}"
        );
    }

    #[test]
    fn a_cname_elsewhere_fails() {
        let answers = DnsAnswers {
            cname_chain: vec!["team.github.io".into()],
            addresses: vec!["185.199.108.153".parse().unwrap()],
        };
        let error = points_at_edge(&host("status.team.dev"), &answers, &edge()).unwrap_err();
        assert!(error.contains("team.github.io"), "{error}");
    }

    #[test]
    fn unresolvable_names_fail() {
        let error =
            points_at_edge(&host("status.team.dev"), &DnsAnswers::default(), &edge()).unwrap_err();
        assert!(error.contains("does not resolve"), "{error}");
    }

    #[test]
    fn without_edge_addresses_only_cnames_verify() {
        let edge = Edge {
            addresses: vec![],
            ..edge()
        };
        let answers = DnsAnswers {
            cname_chain: vec![],
            addresses: vec!["203.0.113.10".parse().unwrap()],
        };
        assert!(points_at_edge(&host("team.dev"), &answers, &edge).is_err());
    }
}
