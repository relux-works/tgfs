//! Secret material and its redaction, plus the keychain seam.
//!
//! Two kinds of value in this module never appear in a log, a `Debug`
//! render, or a diagnostic string: the Telegram `api_hash` ([`Secret`]) and
//! the TDLib database encryption key ([`DatabaseKey`]). Both wrap their
//! bytes and hand the plaintext back only through crate-private accessors
//! that exactly two call sites use — the JSON request builders in the parent
//! module, which put the value on the wire to TDLib and nowhere else
//! (SEC-020, SEC-023, and the checklist's "never logged").
//!
//! The values do not live in this crate. [`SecretSource`] is the seam
//! between the configuration layer and platform secure storage: the native
//! adapter implements it over the OS keychain (macOS Keychain service
//! `gramdrive-telegram`, Android Keystore, Windows DPAPI, Linux Secret
//! Service — SEC-002/SEC-003), and never hands key material to a core crate
//! except through this trait, resolved at runtime. [`InMemorySecrets`] is
//! the artifact-free implementation the tests and CI (env-injected
//! credentials) run against.

use std::collections::HashMap;
use std::fmt;

use gramdrive_model::identity::AccountId;

/// A secret string whose `Debug` and `Display` never reveal the value.
///
/// Wraps the Telegram `api_hash` and proxy credentials. The plaintext is
/// reachable only through the crate-private [`Secret::expose`], which the
/// JSON request builders use to place the value on the wire to TDLib; there
/// is deliberately no public getter and no `Display`, so a secret cannot
/// reach a log by accident.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value` as a secret. The value is copied in and never copied
    /// out except onto the TDLib wire.
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The plaintext, for the JSON request builders only. Crate-private on
    /// purpose: the request this value lands in goes straight to TDLib, and
    /// no other call site may reach the bytes.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The TDLib database encryption key (`setTdlibParameters.database_encryption_key`).
///
/// Held as raw bytes and base64-encoded onto the wire, because TDLib's JSON
/// interface encodes every `bytes`-typed field as base64 — passing the raw
/// bytes as a string would key the database off a different value than
/// intended. Like [`Secret`], the bytes leave only through a crate-private
/// accessor and never through `Debug`.
#[derive(Clone)]
pub struct DatabaseKey(Vec<u8>);

impl DatabaseKey {
    /// Wraps `bytes` as the database encryption key.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> DatabaseKey {
        DatabaseKey(bytes.into())
    }

    /// The base64 form TDLib's JSON `bytes` encoding expects. Crate-private:
    /// the only caller is the `setTdlibParameters` builder.
    pub(crate) fn base64(&self) -> String {
        base64_encode(&self.0)
    }
}

impl fmt::Debug for DatabaseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseKey(<redacted>)")
    }
}

/// The Telegram product API credentials (`api_id`/`api_hash`, SEC-030).
///
/// Both fields are treated as never-logged material (the checklist's
/// "never hardcoded/logged"): the manual `Debug` redacts the whole record,
/// including `api_id`, so neither half reaches a diagnostic string.
#[derive(Clone)]
pub struct ApiCredentials {
    /// The product `api_id`. Not printed by `Debug`.
    pub api_id: i32,
    /// The product `api_hash`. Secret, never printed.
    pub api_hash: Secret,
}

impl fmt::Debug for ApiCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiCredentials(<redacted>)")
    }
}

/// Why resolving a secret from a [`SecretSource`] failed.
///
/// Deliberately carries no secret material: `detail` describes the backend
/// failure (a keychain error class, a missing item), never a key value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// The requested item was absent from the backing store.
    NotFound {
        /// Which item was missing — a fixed label, never a secret value.
        what: &'static str,
    },
    /// The backing store failed for a reason other than absence.
    Backend {
        /// Backend-provided detail; carries no secret material.
        detail: String,
    },
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::NotFound { what } => write!(f, "secret not found: {what}"),
            SecretError::Backend { detail } => write!(f, "secret backend error: {detail}"),
        }
    }
}

impl std::error::Error for SecretError {}

/// The seam between the configuration layer and platform secure storage.
///
/// The native adapter implements this over the OS keychain (SEC-002/SEC-003);
/// the configuration layer calls it at runtime to attach credentials to an
/// otherwise secret-free plan. No implementation of this trait belongs in a
/// core crate — the keychain APIs are platform code — so this crate ships
/// only the trait and the artifact-free [`InMemorySecrets`].
pub trait SecretSource {
    /// The product `api_id`/`api_hash` (macOS: Keychain service
    /// `gramdrive-telegram`).
    fn api_credentials(&self) -> Result<ApiCredentials, SecretError>;

    /// The per-account TDLib database encryption key. A distinct key per
    /// account is what makes each account's database independently
    /// unreadable (SEC-003) and independently disposable (SEC-004).
    fn database_key(&self, account: AccountId) -> Result<DatabaseKey, SecretError>;
}

/// An in-process [`SecretSource`] for tests and CI.
///
/// Production resolves secrets from the platform keychain; this holds them
/// in memory instead (CI injects them from repository secrets, tests from
/// fixtures). It is a legitimate product type, not test-only code: it keeps
/// the artifact-free build paths — every gate that never touches a keychain
/// — able to exercise the full configuration flow.
#[derive(Debug, Clone)]
pub struct InMemorySecrets {
    api: ApiCredentials,
    keys: HashMap<i64, DatabaseKey>,
}

impl InMemorySecrets {
    /// A source carrying only the product credentials; per-account keys are
    /// added with [`InMemorySecrets::with_key`].
    pub fn new(api: ApiCredentials) -> InMemorySecrets {
        InMemorySecrets {
            api,
            keys: HashMap::new(),
        }
    }

    /// Registers the database key for `account`, replacing any prior one.
    pub fn with_key(mut self, account: AccountId, key: DatabaseKey) -> InMemorySecrets {
        self.keys.insert(account.0, key);
        self
    }
}

impl SecretSource for InMemorySecrets {
    fn api_credentials(&self) -> Result<ApiCredentials, SecretError> {
        Ok(self.api.clone())
    }

    fn database_key(&self, account: AccountId) -> Result<DatabaseKey, SecretError> {
        self.keys
            .get(&account.0)
            .cloned()
            .ok_or(SecretError::NotFound {
                what: "database_encryption_key",
            })
    }
}

/// Standard base64 (RFC 4648, padded) — the encoding TDLib's JSON interface
/// uses for every `bytes`-typed field. Implemented here rather than pulled
/// in as a dependency: it is a dozen lines with no configuration surface,
/// and adding a crate to the graph would move the POL-6 supply-chain gate
/// for no benefit.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        // The canonical RFC 4648 §10 vectors, which pin every padding case.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_covers_the_high_alphabet_and_all_byte_values() {
        // 0xFB,0xFF,0xBF exercises the '+' and '/' end of the alphabet.
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xbf]), "+/+/");
        // A full 0..=255 sweep stays byte-clean (no panic, length is the
        // padded ceiling).
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = base64_encode(&all);
        assert_eq!(encoded.len(), all.len().div_ceil(3) * 4);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("api-hash-sentinel");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert!(!format!("{secret:?}").contains("sentinel"));
        // The plaintext is still reachable for the wire.
        assert_eq!(secret.expose(), "api-hash-sentinel");
    }

    #[test]
    fn database_key_debug_is_redacted_but_base64_round_trips() {
        let key = DatabaseKey::from_bytes(b"foobar".to_vec());
        assert_eq!(format!("{key:?}"), "DatabaseKey(<redacted>)");
        assert_eq!(key.base64(), "Zm9vYmFy");
    }

    #[test]
    fn api_credentials_debug_hides_both_halves() {
        let creds = ApiCredentials {
            api_id: 4242,
            api_hash: Secret::new("hash-sentinel"),
        };
        let rendered = format!("{creds:?}");
        assert_eq!(rendered, "ApiCredentials(<redacted>)");
        assert!(!rendered.contains("4242"));
        assert!(!rendered.contains("sentinel"));
    }

    #[test]
    fn in_memory_source_resolves_registered_key_and_reports_missing() {
        let creds = ApiCredentials {
            api_id: 7,
            api_hash: Secret::new("h"),
        };
        let present = AccountId(10);
        let absent = AccountId(11);
        let source =
            InMemorySecrets::new(creds).with_key(present, DatabaseKey::from_bytes(vec![1]));

        assert_eq!(source.api_credentials().unwrap().api_id, 7);
        assert_eq!(source.database_key(present).unwrap().base64(), "AQ==");
        // Secret types intentionally carry no `PartialEq`, so match the
        // error rather than comparing the `Result`.
        assert!(matches!(
            source.database_key(absent),
            Err(SecretError::NotFound {
                what: "database_encryption_key"
            })
        ));
    }
}
