//! Secrets and sensitive data, encrypted at rest.
//!
//! A [`Keyring`] holds named 256-bit keys (`secrets.keys`): the first encrypts,
//! the others only decrypt, so keys can be rotated. Values are sealed with
//! XChaCha20-Poly1305 under a random 24-byte nonce, with their column
//! (`"table.column"`) as associated data, so a ciphertext cannot be moved to
//! another column. The stored form is
//! `enc:v1:<key id>:<base64url(nonce || ciphertext)>`.
//!
//! Inside the store, every sensitive column goes through the [`Sealer`]:
//!
//! - write `sealer.seal("table.column", &value)?` and read
//!   `sealer.open("table.column", &stored)?` (plus `seal_opt`/`open_opt`);
//! - add the column to [`SEALED_COLUMNS`] so existing plaintext is sealed at
//!   startup and `secrets rotate` re-encrypts it;
//! - for values that must be looked up (e.g. a subscriber's email or a push
//!   token), store [`Sealer::blind_index`] in an indexed column next to the
//!   sealed value and query with every entry of [`Sealer::blind_indexes`]
//!   (one per key, so lookups keep working during rotation); then compare the
//!   opened value. Indexes are keyed HMAC-SHA256, so they reveal nothing
//!   without the key; recompute them in `Store::reseal` when adding one.
//!
//! Without a keyring (debug builds only) values are stored as plaintext and
//! sealed on the first start with a key. Plaintext is always readable, so a
//! database written before encryption keeps working until it is sealed.

use std::{fmt, sync::Arc};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead as _, KeyInit as _, Payload},
};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

/// Marks a sealed value; the version names the format and cipher.
const PREFIX: &str = "enc:v1:";
const NONCE_LEN: usize = 24;
/// Index key for databases without a keyring (debug builds): no secrecy.
const UNKEYED_INDEX: [u8; 32] = [0; 32];

/// Sealed columns, as `(table, column)`, sealed at startup and re-encrypted
/// by `secrets rotate`. The associated data is `"table.column"`.
pub const SEALED_COLUMNS: &[(&str, &str)] = &[
    ("monitors", "check"),
    ("notification_channels", "url"),
    ("notification_channels", "secret"),
    ("check_results", "response_body"),
    ("incident_updates", "check_json"),
];

/// A malformed `secrets.keys` value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyringError {
    #[error("secrets.keys is empty")]
    Empty,
    #[error("secrets.keys entries must look like `id:base64key` (got `{0}`)")]
    Malformed(String),
    #[error("secret key `{0}` must be base64 of exactly 32 bytes")]
    BadKey(String),
    #[error("secret key id `{0}` is used twice")]
    Duplicate(String),
}

/// Why a stored value could not be opened.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    #[error("{0} is encrypted, but no secrets.keys are configured")]
    NoKeyring(String),
    #[error("{column} is encrypted with key `{key}`, which is not in secrets.keys")]
    UnknownKey { column: String, key: String },
    #[error("{0} is not a valid encrypted value")]
    Malformed(String),
    #[error("{0} could not be decrypted (wrong key, other column, or tampered)")]
    Rejected(String),
}

#[derive(Clone)]
struct Key {
    id: String,
    cipher: XChaCha20Poly1305,
    index: [u8; 32],
}

/// Named encryption keys; the first is active.
#[derive(Clone)]
pub struct Keyring {
    keys: Vec<Key>,
}

impl fmt::Debug for Keyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<&str> = self.keys.iter().map(|k| k.id.as_str()).collect();
        f.debug_struct("Keyring").field("ids", &ids).finish()
    }
}

impl Keyring {
    /// Parses `id:base64,id:base64,…` (the first key is active).
    pub fn parse(spec: &str) -> Result<Self, KeyringError> {
        let mut keys: Vec<Key> = Vec::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (id, encoded) = entry
                .split_once(':')
                .filter(|(id, _)| valid_id(id))
                .ok_or_else(|| KeyringError::Malformed(redact(entry)))?;
            let bytes: [u8; 32] = STANDARD
                .decode(encoded.trim())
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| KeyringError::BadKey(id.to_owned()))?;
            if keys.iter().any(|k| k.id == id) {
                return Err(KeyringError::Duplicate(id.to_owned()));
            }
            keys.push(Key::new(id, &bytes));
        }
        if keys.is_empty() {
            return Err(KeyringError::Empty);
        }
        Ok(Self { keys })
    }

    /// A new random key as a `secrets.keys` entry: `k<8 hex>:<base64>`.
    pub fn generate() -> String {
        let mut bytes = [0u8; 36];
        // The OS RNG is required for keys; there is no safe fallback.
        #[allow(clippy::expect_used)]
        getrandom::fill(&mut bytes).expect("the OS random number generator is available");
        format!(
            "k{}:{}",
            hex::encode(&bytes[..4]),
            STANDARD.encode(&bytes[4..])
        )
    }

    /// The id of the key that encrypts new values.
    pub fn active_id(&self) -> &str {
        &self.active().id
    }

    fn active(&self) -> &Key {
        &self.keys[0]
    }

    fn key(&self, id: &str) -> Option<&Key> {
        self.keys.iter().find(|k| k.id == id)
    }

    /// Encrypts `plaintext` for `column` with the active key.
    pub fn encrypt(&self, column: &str, plaintext: &str) -> String {
        let key = self.active();
        let mut nonce = [0u8; NONCE_LEN];
        #[allow(clippy::expect_used)]
        getrandom::fill(&mut nonce).expect("the OS random number generator is available");
        let payload = Payload {
            msg: plaintext.as_bytes(),
            aad: column.as_bytes(),
        };
        // Encryption only fails for messages beyond the cipher's 256 GiB limit.
        #[allow(clippy::expect_used)]
        let sealed = key
            .cipher
            .encrypt(&XNonce::from(nonce), payload)
            .expect("value fits the cipher's length limit");
        let mut body = nonce.to_vec();
        body.extend_from_slice(&sealed);
        format!("{PREFIX}{}:{}", key.id, URL_SAFE_NO_PAD.encode(body))
    }

    /// Decrypts a value [`Keyring::encrypt`] produced for `column`.
    pub fn decrypt(&self, column: &str, stored: &str) -> Result<String, OpenError> {
        let rest = stored
            .strip_prefix(PREFIX)
            .ok_or_else(|| OpenError::Malformed(column.to_owned()))?;
        let (id, body) = rest
            .split_once(':')
            .ok_or_else(|| OpenError::Malformed(column.to_owned()))?;
        let key = self.key(id).ok_or_else(|| OpenError::UnknownKey {
            column: column.to_owned(),
            key: id.to_owned(),
        })?;
        let body = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| OpenError::Malformed(column.to_owned()))?;
        if body.len() < NONCE_LEN {
            return Err(OpenError::Malformed(column.to_owned()));
        }
        let (nonce, sealed) = body.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| OpenError::Malformed(column.to_owned()))?;
        let plain = key
            .cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: sealed,
                    aad: column.as_bytes(),
                },
            )
            .map_err(|_| OpenError::Rejected(column.to_owned()))?;
        String::from_utf8(plain).map_err(|_| OpenError::Rejected(column.to_owned()))
    }

    /// Whether `stored` is sealed with a key other than the active one.
    fn is_stale(&self, stored: &str) -> bool {
        sealed_key_id(stored).is_some_and(|id| id != self.active_id())
    }
}

impl Key {
    fn new(id: &str, bytes: &[u8; 32]) -> Self {
        Self {
            id: id.to_owned(),
            cipher: XChaCha20Poly1305::new(&(*bytes).into()),
            index: hmac(bytes, b"uptimestatus blind index v1"),
        }
    }
}

/// The store's view of the keyring: seals and opens column values, and
/// computes blind indexes. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Sealer(Option<Arc<Keyring>>);

impl Sealer {
    pub fn new(keyring: Option<Keyring>) -> Self {
        Self(keyring.map(Arc::new))
    }

    pub fn keyring(&self) -> Option<&Keyring> {
        self.0.as_deref()
    }

    /// The stored form of `plaintext` (plaintext itself without a keyring).
    pub fn seal(&self, column: &str, plaintext: &str) -> String {
        match &self.0 {
            Some(keyring) => keyring.encrypt(column, plaintext),
            None => plaintext.to_owned(),
        }
    }

    pub fn seal_opt(&self, column: &str, plaintext: Option<&str>) -> Option<String> {
        plaintext.map(|p| self.seal(column, p))
    }

    /// The plaintext of a stored value. Unsealed (legacy) values pass through.
    pub fn open(&self, column: &str, stored: &str) -> Result<String, OpenError> {
        if !is_sealed(stored) {
            return Ok(stored.to_owned());
        }
        match &self.0 {
            Some(keyring) => keyring.decrypt(column, stored),
            None => Err(OpenError::NoKeyring(column.to_owned())),
        }
    }

    pub fn open_opt(
        &self,
        column: &str,
        stored: Option<&str>,
    ) -> Result<Option<String>, OpenError> {
        stored.map(|s| self.open(column, s)).transpose()
    }

    /// Whether `stored` needs sealing with the active key (plaintext, or
    /// sealed with an older key). Always false without a keyring.
    pub fn needs_seal(&self, stored: &str) -> bool {
        match &self.0 {
            Some(keyring) => !is_sealed(stored) || keyring.is_stale(stored),
            None => false,
        }
    }

    /// The lookup token for `value` under the active key, to store.
    pub fn blind_index(&self, purpose: &str, value: &str) -> String {
        let key = self
            .0
            .as_ref()
            .map_or(&UNKEYED_INDEX, |k| &k.active().index);
        index(key, purpose, value)
    }

    /// The lookup tokens for `value` under every key, to query with.
    pub fn blind_indexes(&self, purpose: &str, value: &str) -> Vec<String> {
        match &self.0 {
            Some(keyring) => keyring
                .keys
                .iter()
                .map(|k| index(&k.index, purpose, value))
                .collect(),
            None => vec![index(&UNKEYED_INDEX, purpose, value)],
        }
    }
}

/// Whether a stored value is in the sealed format.
pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

fn sealed_key_id(stored: &str) -> Option<&str> {
    stored
        .strip_prefix(PREFIX)?
        .split_once(':')
        .map(|(id, _)| id)
}

fn index(key: &[u8; 32], purpose: &str, value: &str) -> String {
    let mut input = Vec::with_capacity(purpose.len() + 1 + value.len());
    input.extend_from_slice(purpose.as_bytes());
    input.push(0);
    input.extend_from_slice(value.as_bytes());
    hex::encode(hmac(key, &input))
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length.
    #[allow(clippy::expect_used)]
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An entry for error messages, without its key material.
fn redact(entry: &str) -> String {
    entry
        .split_once(':')
        .map_or_else(|| "…".to_owned(), |(id, _)| format!("{id}:…"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pretty_assertions::assert_eq;

    use super::*;

    fn key(id: &str, byte: u8) -> String {
        format!("{id}:{}", STANDARD.encode([byte; 32]))
    }

    fn ring(spec: &[(&str, u8)]) -> Keyring {
        let entries: Vec<String> = spec.iter().map(|(id, b)| key(id, *b)).collect();
        Keyring::parse(&entries.join(",")).unwrap()
    }

    #[test]
    fn round_trips_with_a_fresh_nonce_each_time() {
        let keys = ring(&[("a", 1)]);
        let first = keys.encrypt("monitors.check", r#"{"token":"s3cret"}"#);
        let second = keys.encrypt("monitors.check", r#"{"token":"s3cret"}"#);
        assert!(first.starts_with("enc:v1:a:"));
        assert_ne!(first, second, "random nonces");
        assert!(!first.contains("s3cret"));
        assert_eq!(
            keys.decrypt("monitors.check", &first).unwrap(),
            r#"{"token":"s3cret"}"#
        );
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let sealed = ring(&[("a", 1)]).encrypt("c.x", "secret");
        assert_eq!(
            ring(&[("a", 2)]).decrypt("c.x", &sealed),
            Err(OpenError::Rejected("c.x".into()))
        );
        assert_eq!(
            ring(&[("b", 1)]).decrypt("c.x", &sealed),
            Err(OpenError::UnknownKey {
                column: "c.x".into(),
                key: "a".into()
            })
        );
    }

    #[test]
    fn tampering_is_detected() {
        let keys = ring(&[("a", 1)]);
        let sealed = keys.encrypt("c.x", "secret");
        let (head, body) = sealed.rsplit_once(':').unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(body).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let tampered = format!("{head}:{}", URL_SAFE_NO_PAD.encode(bytes));
        assert_eq!(
            keys.decrypt("c.x", &tampered),
            Err(OpenError::Rejected("c.x".into()))
        );
        assert_eq!(
            keys.decrypt("c.x", "enc:v1:a:@@"),
            Err(OpenError::Malformed("c.x".into()))
        );
    }

    #[test]
    fn values_cannot_move_between_columns() {
        let keys = ring(&[("a", 1)]);
        let sealed = keys.encrypt("notification_channels.url", "https://hooks.example/x");
        assert_eq!(
            keys.decrypt("notification_channels.secret", &sealed),
            Err(OpenError::Rejected("notification_channels.secret".into()))
        );
    }

    #[test]
    fn rotation_reads_old_keys_and_writes_the_new_one() {
        let old = ring(&[("old", 1)]);
        let sealed = old.encrypt("c.x", "secret");
        let rotated = Sealer::new(Some(ring(&[("new", 2), ("old", 1)])));
        assert_eq!(rotated.open("c.x", &sealed).unwrap(), "secret");
        assert!(rotated.needs_seal(&sealed), "sealed with an older key");
        let resealed = rotated.seal("c.x", "secret");
        assert!(resealed.starts_with("enc:v1:new:"));
        assert!(!rotated.needs_seal(&resealed));
        // Lookups match indexes written under either key.
        let written = Sealer::new(Some(old)).blind_index("push", "tok");
        assert!(rotated.blind_indexes("push", "tok").contains(&written));
        assert_eq!(rotated.blind_indexes("push", "tok").len(), 2);
    }

    #[test]
    fn plaintext_passes_through_and_needs_sealing() {
        let keyed = Sealer::new(Some(ring(&[("a", 1)])));
        assert_eq!(keyed.open("c.x", "legacy").unwrap(), "legacy");
        assert!(keyed.needs_seal("legacy"));
        let unkeyed = Sealer::default();
        assert_eq!(unkeyed.seal("c.x", "plain"), "plain");
        assert!(!unkeyed.needs_seal("plain"));
        let sealed = keyed.seal("c.x", "secret");
        assert_eq!(
            unkeyed.open("c.x", &sealed),
            Err(OpenError::NoKeyring("c.x".into()))
        );
    }

    #[test]
    fn blind_indexes_are_keyed_and_purpose_bound() {
        let a = Sealer::new(Some(ring(&[("a", 1)])));
        let b = Sealer::new(Some(ring(&[("b", 2)])));
        assert_eq!(
            a.blind_index("email", "x@y.z"),
            a.blind_index("email", "x@y.z")
        );
        assert_ne!(
            a.blind_index("email", "x@y.z"),
            b.blind_index("email", "x@y.z")
        );
        assert_ne!(
            a.blind_index("email", "x@y.z"),
            a.blind_index("push", "x@y.z")
        );
        assert_eq!(a.blind_index("email", "x@y.z").len(), 64);
    }

    #[test]
    fn production_style_keys_parse() {
        // `k<date>:` and standard base64 with `=` padding, as generated for production.
        let encoded = STANDARD.encode([42u8; 32]);
        assert!(encoded.ends_with('='));
        let keys = Keyring::parse(&format!("k20261015:{encoded}")).unwrap();
        assert_eq!(keys.active_id(), "k20261015");
        let sealed = keys.encrypt("c.x", "secret");
        assert!(sealed.starts_with("enc:v1:k20261015:"));
        assert_eq!(keys.decrypt("c.x", &sealed).unwrap(), "secret");
    }

    #[test]
    fn keyrings_parse_strictly() {
        assert_eq!(Keyring::parse(" ").unwrap_err(), KeyringError::Empty);
        assert_eq!(
            Keyring::parse("nokey").unwrap_err(),
            KeyringError::Malformed("…".into())
        );
        assert_eq!(
            Keyring::parse("a:c2hvcnQ=").unwrap_err(),
            KeyringError::BadKey("a".into())
        );
        assert_eq!(
            Keyring::parse(&format!("{},{}", key("a", 1), key("a", 2))).unwrap_err(),
            KeyringError::Duplicate("a".into())
        );
        let generated = Keyring::generate();
        let parsed = Keyring::parse(&format!("{generated}, {}", key("old", 3))).unwrap();
        assert_eq!(parsed.active_id(), generated.split(':').next().unwrap());
        assert!(!format!("{parsed:?}").contains(&generated[generated.len() - 8..]));
    }
}
