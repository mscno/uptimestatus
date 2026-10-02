//! Accepted HTTP status code ranges, e.g. `"200-299,403"`.

use std::{fmt, ops::RangeInclusive, str::FromStr};

use serde::{Deserialize, Serialize};

/// A non-empty set of HTTP status codes that count as a successful check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StatusRanges(Vec<RangeInclusive<u16>>);

/// Why a status range expression could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatusRangesError {
    #[error("at least one status code or range is required")]
    Empty,
    #[error("`{0}` is not a valid status code")]
    InvalidCode(String),
    #[error("status code {0} is outside 100-599")]
    OutOfRange(u16),
    #[error("range `{0}` ends before it starts")]
    Reversed(String),
}

impl StatusRanges {
    /// Any `2xx` response.
    pub fn success() -> Self {
        #[allow(clippy::single_range_in_vec_init)] // one range, not a list of 100 codes
        Self(vec![200..=299])
    }

    /// Whether `code` falls inside any of the ranges.
    pub fn contains(&self, code: u16) -> bool {
        self.0.iter().any(|range| range.contains(&code))
    }

    fn parse_code(input: &str) -> Result<u16, StatusRangesError> {
        let code: u16 = input
            .trim()
            .parse()
            .map_err(|_| StatusRangesError::InvalidCode(input.trim().to_owned()))?;
        if (100..=599).contains(&code) {
            Ok(code)
        } else {
            Err(StatusRangesError::OutOfRange(code))
        }
    }
}

impl Default for StatusRanges {
    fn default() -> Self {
        Self::success()
    }
}

impl FromStr for StatusRanges {
    type Err = StatusRangesError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ranges = s
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| match part.split_once('-') {
                None => Self::parse_code(part).map(|code| code..=code),
                Some((start, end)) => {
                    let (start, end) = (Self::parse_code(start)?, Self::parse_code(end)?);
                    if start > end {
                        return Err(StatusRangesError::Reversed(part.replace(' ', "")));
                    }
                    Ok(start..=end)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ranges.is_empty() {
            return Err(StatusRangesError::Empty);
        }
        Ok(Self(ranges))
    }
}

impl fmt::Display for StatusRanges {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, range) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            if range.start() == range.end() {
                write!(f, "{}", range.start())?;
            } else {
                write!(f, "{}-{}", range.start(), range.end())?;
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for StatusRanges {
    type Error = StatusRangesError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<StatusRanges> for String {
    fn from(value: StatusRanges) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn default_accepts_any_2xx_only() {
        let ranges = StatusRanges::default();
        assert!(ranges.contains(200));
        assert!(ranges.contains(204));
        assert!(ranges.contains(299));
        assert!(!ranges.contains(199));
        assert!(!ranges.contains(300));
        assert!(!ranges.contains(403));
    }

    #[rstest]
    #[case("403", &[403], &[200, 401, 404])]
    #[case("200-299,403", &[200, 250, 299, 403], &[300, 401, 500])]
    #[case(" 200 - 204 , 301 ", &[200, 204, 301], &[205, 302])]
    #[case("500-599", &[500, 503, 599], &[499])]
    fn parses_and_matches(#[case] input: &str, #[case] hits: &[u16], #[case] misses: &[u16]) {
        let ranges: StatusRanges = input.parse().unwrap();
        for code in hits {
            assert!(ranges.contains(*code), "{input} should contain {code}");
        }
        for code in misses {
            assert!(!ranges.contains(*code), "{input} should not contain {code}");
        }
    }

    #[rstest]
    #[case("", StatusRangesError::Empty)]
    #[case(" , ", StatusRangesError::Empty)]
    #[case("abc", StatusRangesError::InvalidCode("abc".into()))]
    #[case("200-", StatusRangesError::InvalidCode("".into()))]
    #[case("99", StatusRangesError::OutOfRange(99))]
    #[case("200-600", StatusRangesError::OutOfRange(600))]
    #[case("299-200", StatusRangesError::Reversed("299-200".into()))]
    fn rejects_invalid_input(#[case] input: &str, #[case] expected: StatusRangesError) {
        assert_eq!(input.parse::<StatusRanges>(), Err(expected));
    }

    #[test]
    fn displays_in_canonical_form() {
        let ranges: StatusRanges = " 200 - 299 , 403, 404-404 ".parse().unwrap();
        assert_eq!(ranges.to_string(), "200-299,403,404");
    }

    #[test]
    fn serde_round_trips_as_a_string() {
        let ranges: StatusRanges = "200-299,403".parse().unwrap();
        let json = serde_json::to_string(&ranges).unwrap();
        assert_eq!(json, r#""200-299,403""#);
        let back: StatusRanges = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ranges);
    }

    #[test]
    fn serde_rejects_invalid_strings() {
        assert!(serde_json::from_str::<StatusRanges>(r#""nope""#).is_err());
    }
}
