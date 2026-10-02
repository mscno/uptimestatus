//! Monitor identity and definition.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{CheckPolicy, CheckSpec};

/// Database identity of a monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonitorId(pub i64);

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable, human-chosen slug that page definitions refer to, e.g. `api-health`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MonitorKey(String);

/// Why a string is not a valid [`MonitorKey`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "monitor key must be 1-64 chars of a-z, 0-9 and '-', not starting or ending with '-': `{0}`"
)]
pub struct MonitorKeyError(pub String);

impl MonitorKey {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for MonitorKey {
    type Err = MonitorKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid_chars = s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let valid = !s.is_empty()
            && s.len() <= Self::MAX_LEN
            && valid_chars
            && !s.starts_with('-')
            && !s.ends_with('-');
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(MonitorKeyError(s.to_owned()))
        }
    }
}

impl TryFrom<String> for MonitorKey {
    type Error = MonitorKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<MonitorKey> for String {
    fn from(value: MonitorKey) -> Self {
        value.0
    }
}

impl fmt::Display for MonitorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything an admin configures about a monitor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorSpec {
    pub key: MonitorKey,
    pub name: String,
    pub check: CheckSpec,
    #[serde(default)]
    pub policy: CheckPolicy,
    #[serde(default = "MonitorSpec::default_active")]
    pub active: bool,
    /// Free-form labels for grouping and filtering, normalized (see [`normalize_tags`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Where the monitor sits in the tree of groups, e.g. `prod/eu` (see [`crate::group`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl MonitorSpec {
    const fn default_active() -> bool {
        true
    }
}

/// The most tags a monitor carries.
pub const MAX_TAGS: usize = 10;
/// The longest tag.
pub const MAX_TAG_LEN: usize = 32;

/// Why a tag list was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    #[error("tags may use a-z, 0-9, `-`, `_` and `:` (up to 32 characters): `{0}`")]
    Invalid(String),
    #[error("a monitor can have at most 10 tags")]
    TooMany,
}

/// Parses `prod, api-tier` (commas or whitespace): lowercased, sorted, de-duplicated.
pub fn normalize_tags(input: &str) -> Result<Vec<String>, TagError> {
    let mut tags: Vec<String> = Vec::new();
    for raw in input.split(|c: char| c == ',' || c.is_whitespace()) {
        let tag = raw.trim().to_lowercase();
        if tag.is_empty() {
            continue;
        }
        let valid = tag.len() <= MAX_TAG_LEN
            && tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_:".contains(&b));
        if !valid {
            return Err(TagError::Invalid(tag));
        }
        tags.push(tag);
    }
    tags.sort();
    tags.dedup();
    if tags.len() > MAX_TAGS {
        return Err(TagError::TooMany);
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("api")]
    #[case("api-health")]
    #[case("db-2")]
    #[case("a")]
    fn accepts_valid_keys(#[case] input: &str) {
        assert_eq!(input.parse::<MonitorKey>().unwrap().as_str(), input);
    }

    #[rstest]
    #[case("")]
    #[case("-api")]
    #[case("api-")]
    #[case("Api")]
    #[case("api health")]
    #[case("api_health")]
    #[case("ø")]
    fn rejects_invalid_keys(#[case] input: &str) {
        assert_eq!(
            input.parse::<MonitorKey>(),
            Err(MonitorKeyError(input.into()))
        );
    }

    #[test]
    fn tags_are_normalized() {
        assert_eq!(
            normalize_tags("Prod, api  prod\nteam:core"),
            Ok(vec!["api".into(), "prod".into(), "team:core".into()])
        );
        assert_eq!(normalize_tags(" , "), Ok(vec![]));
    }

    #[test]
    fn bad_tags_are_refused() {
        assert_eq!(
            normalize_tags("ok, no way!"),
            Err(TagError::Invalid("way!".into()))
        );
        assert!(matches!(
            normalize_tags(&"x".repeat(33)),
            Err(TagError::Invalid(_))
        ));
        let many = (0..11)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(normalize_tags(&many), Err(TagError::TooMany));
    }

    #[test]
    fn rejects_keys_longer_than_64_chars() {
        let long = "a".repeat(65);
        assert!(long.parse::<MonitorKey>().is_err());
        assert!("a".repeat(64).parse::<MonitorKey>().is_ok());
    }

    #[test]
    fn spec_defaults_to_active_with_default_policy() {
        let spec: MonitorSpec = serde_json::from_str(
            r#"{"key":"api","name":"API","check":{"type":"tcp","host":"api.example.com","port":443}}"#,
        )
        .unwrap();
        assert!(spec.active);
        assert_eq!(spec.policy, CheckPolicy::default());
    }

    #[test]
    fn spec_rejects_unknown_fields() {
        let json = r#"{"key":"api","name":"API","colour":"red","check":{"type":"tcp","host":"h","port":1}}"#;
        let error = serde_json::from_str::<MonitorSpec>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("colour"), "{error}");
    }
}
