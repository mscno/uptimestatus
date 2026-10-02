//! Who may sign in to the admin console.
//!
//! Admins are GitHub users on an allowlist of usernames. Because GitHub
//! usernames can be renamed and then re-registered by someone else, the first
//! successful login pins the username to its immutable numeric GitHub id; later
//! logins must match both.

use std::{collections::BTreeSet, fmt, str::FromStr};

/// The configured set of GitHub usernames allowed to sign in (case-insensitive).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist(BTreeSet<String>);

impl Allowlist {
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(logins: I) -> Self {
        Self(
            logins
                .into_iter()
                .map(|login| login.as_ref().trim().to_ascii_lowercase())
                .filter(|login| !login.is_empty())
                .collect(),
        )
    }

    pub fn contains(&self, login: &str) -> bool {
        self.0.contains(&login.trim().to_ascii_lowercase())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl FromStr for Allowlist {
    type Err = std::convert::Infallible;

    /// Comma- or whitespace-separated usernames: `"alice, bob carol"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s.split(|c: char| c == ',' || c.is_whitespace())))
    }
}

/// Why a GitHub identity may not sign in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginDenied {
    #[error("{login} is not on the admin allowlist")]
    NotAllowlisted { login: String },
    #[error("{login} is pinned to a different GitHub account")]
    IdentityMismatch { login: String },
}

/// Decides whether `login` (GitHub user `github_id`) may sign in.
///
/// `pinned_id` is the GitHub id previously recorded for this username, if any.
pub fn authorize(
    allowlist: &Allowlist,
    login: &str,
    github_id: i64,
    pinned_id: Option<i64>,
) -> Result<(), LoginDenied> {
    let login = login.to_ascii_lowercase();
    if !allowlist.contains(&login) {
        return Err(LoginDenied::NotAllowlisted { login });
    }
    match pinned_id {
        Some(pinned) if pinned != github_id => Err(LoginDenied::IdentityMismatch { login }),
        _ => Ok(()),
    }
}

impl fmt::Display for Allowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.iter().cloned().collect::<Vec<_>>().join(","))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn allowlist() -> Allowlist {
        "Alice, bob\ncarol".parse().unwrap()
    }

    #[test]
    fn parses_comma_and_whitespace_separated_usernames_case_insensitively() {
        let list = allowlist();
        assert!(list.contains("alice"));
        assert!(list.contains("ALICE"));
        assert!(list.contains("Bob"));
        assert!(list.contains("carol"));
        assert!(!list.contains("mallory"));
        assert_eq!(list.iter().collect::<Vec<_>>(), ["alice", "bob", "carol"]);
    }

    #[test]
    fn empty_entries_are_ignored() {
        let list: Allowlist = " , ,alice,,".parse().unwrap();
        assert_eq!(list.iter().collect::<Vec<_>>(), ["alice"]);
        assert!(Allowlist::default().is_empty());
    }

    #[test]
    fn first_login_of_an_allowlisted_user_is_allowed() {
        assert_eq!(authorize(&allowlist(), "alice", 1001, None), Ok(()));
    }

    #[test]
    fn returning_user_with_the_pinned_id_is_allowed() {
        assert_eq!(authorize(&allowlist(), "Alice", 1001, Some(1001)), Ok(()));
    }

    #[test]
    fn users_off_the_allowlist_are_denied() {
        assert_eq!(
            authorize(&allowlist(), "mallory", 666, None),
            Err(LoginDenied::NotAllowlisted {
                login: "mallory".into()
            })
        );
    }

    #[test]
    fn a_reregistered_username_is_denied() {
        // The original "alice" renamed their account; someone else registered "alice".
        assert_eq!(
            authorize(&allowlist(), "alice", 2002, Some(1001)),
            Err(LoginDenied::IdentityMismatch {
                login: "alice".into()
            })
        );
    }

    #[test]
    fn allowlist_is_checked_before_pinning() {
        assert_eq!(
            authorize(&allowlist(), "mallory", 1001, Some(1001)),
            Err(LoginDenied::NotAllowlisted {
                login: "mallory".into()
            })
        );
    }
}
