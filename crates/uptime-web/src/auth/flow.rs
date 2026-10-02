//! OAuth `state` and PKCE verifier for one login attempt, carried in an
//! encrypted, short-lived cookie between the redirect and the callback.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Flow {
    pub(crate) state: String,
    pub(crate) verifier: String,
}

impl Flow {
    /// A fresh random state (128 bits) and verifier (256 bits).
    pub(crate) fn new() -> Self {
        Self {
            state: random_token(16),
            verifier: random_token(32),
        }
    }

    /// The S256 PKCE challenge for the verifier (RFC 7636).
    pub(crate) fn challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()))
    }

    /// Cookie form: `state.verifier` (base64url never contains `.`).
    pub(crate) fn encode(&self) -> String {
        format!("{}.{}", self.state, self.verifier)
    }

    pub(crate) fn decode(value: &str) -> Option<Self> {
        let (state, verifier) = value.split_once('.')?;
        (!state.is_empty() && !verifier.is_empty()).then(|| Self {
            state: state.to_owned(),
            verifier: verifier.to_owned(),
        })
    }

    /// Whether the state GitHub echoed back is ours (constant time).
    pub(crate) fn matches(&self, returned: &str) -> bool {
        let (a, b) = (self.state.as_bytes(), returned.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
    }
}

#[allow(clippy::expect_used)] // no randomness means no safe logins; fail loudly
fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the OS random number generator is available");
    URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn challenge_matches_the_rfc_7636_example() {
        let flow = Flow {
            state: "s".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        };
        assert_eq!(
            flow.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn fresh_flows_are_random_and_well_formed() {
        let (a, b) = (Flow::new(), Flow::new());
        assert_ne!(a, b);
        assert_eq!(a.state.len(), 22);
        assert_eq!(a.verifier.len(), 43, "PKCE verifiers are 43-128 chars");
    }

    #[test]
    fn round_trips_through_the_cookie_value() {
        let flow = Flow::new();
        assert_eq!(Flow::decode(&flow.encode()), Some(flow));
        assert_eq!(Flow::decode("garbage"), None);
        assert_eq!(Flow::decode(".x"), None);
    }

    #[test]
    fn only_the_exact_state_matches() {
        let flow = Flow {
            state: "abc".into(),
            verifier: "v".into(),
        };
        assert!(flow.matches("abc"));
        assert!(!flow.matches("abd"));
        assert!(!flow.matches("abcd"));
        assert!(!flow.matches(""));
    }
}
