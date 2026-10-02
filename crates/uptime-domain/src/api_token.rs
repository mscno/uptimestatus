//! API tokens: what a bearer token may do on the JSON API.

use std::{fmt, str::FromStr};

/// Every token starts with this, so leaked ones are easy to spot and scan for.
pub const TOKEN_PREFIX: &str = "upt_";

/// What a token may do. Write includes read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TokenScope {
    Read,
    Write,
}

impl TokenScope {
    pub const ALL: &'static [Self] = &[Self::Read, Self::Write];

    /// Whether a token of this scope may do something needing `needed`.
    pub fn permits(self, needed: Self) -> bool {
        self >= needed
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "Read only",
            Self::Write => "Read and write",
        }
    }
}

impl fmt::Display for TokenScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TokenScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| format!("unknown scope `{s}`"))
    }
}

/// The bearer token in an `Authorization` header value, if it is one of ours.
pub fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && token.starts_with(TOKEN_PREFIX)).then_some(token)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn write_includes_read() {
        assert!(TokenScope::Write.permits(TokenScope::Read));
        assert!(TokenScope::Write.permits(TokenScope::Write));
        assert!(TokenScope::Read.permits(TokenScope::Read));
        assert!(!TokenScope::Read.permits(TokenScope::Write));
    }

    #[test]
    fn scopes_round_trip() {
        for scope in TokenScope::ALL {
            assert_eq!(scope.as_str().parse::<TokenScope>(), Ok(*scope));
        }
        assert!("admin".parse::<TokenScope>().is_err());
    }

    #[test]
    fn bearer_tokens_are_extracted() {
        assert_eq!(bearer_token("Bearer upt_abc"), Some("upt_abc"));
        assert_eq!(bearer_token("bearer  upt_abc "), Some("upt_abc"));
        assert_eq!(bearer_token("Basic upt_abc"), None);
        assert_eq!(bearer_token("Bearer other"), None);
        assert_eq!(bearer_token("upt_abc"), None);
    }
}
