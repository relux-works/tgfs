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
use std::sync::Mutex;

use gramdrive_model::identity::AccountId;

/// The length in bytes of a GramDrive-created database encryption key: 256
/// bits, the size TDLib recommends for `database_encryption_key` and the only
/// size [`DatabaseKey::from_entropy`] produces. [`DatabaseKey::from_stored`]
/// enforces it when reading a key back out of secure storage, so a truncated
/// or blanked keychain item fails closed rather than silently weakening the
/// database (SEC-002/SEC-003).
pub const DATABASE_KEY_LEN: usize = 32;

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
    /// Wraps `bytes` as the database encryption key, unvalidated.
    ///
    /// For material whose length GramDrive already controls — a freshly
    /// generated key, a test fixture — and for the TDLib wire, where a
    /// `bytes` field may legitimately be any length. Bytes read back from
    /// untrusted secure storage go through [`DatabaseKey::from_stored`]
    /// instead, which fails closed on a malformed item.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> DatabaseKey {
        DatabaseKey(bytes.into())
    }

    /// Creates a fresh database key from 256 bits of platform entropy — the
    /// *creation* path (SEC-002).
    ///
    /// The native adapter draws `bytes` from the OS CSPRNG (macOS
    /// `SecRandomCopyBytes`, Android `SecureRandom`, …) on first
    /// authorization and persists the result through
    /// [`SecretStore::put_database_key`]. The randomness stays on the
    /// platform side by construction — the core links no CSPRNG — and the
    /// fixed-size array makes a wrong-length key unrepresentable.
    pub fn from_entropy(bytes: [u8; DATABASE_KEY_LEN]) -> DatabaseKey {
        DatabaseKey(bytes.to_vec())
    }

    /// Validates raw bytes read back from secure storage into a key, or
    /// reports [`SecretError::Corrupt`] — never a usable key — when they are
    /// not what GramDrive stored.
    ///
    /// The keychain is outside the app's integrity boundary, so a truncated,
    /// empty, or zeroed item must fail closed. The alternative TDLib offers
    /// for a bad `database_encryption_key` is worse: an *empty* key means
    /// "database not encrypted", a silent plaintext fallback this rejects
    /// outright (the checklist's "no plaintext fallback"). A key is
    /// well-formed only if it is exactly [`DATABASE_KEY_LEN`] bytes and not
    /// all-zero (the shape a blanked item takes).
    pub fn from_stored(bytes: &[u8]) -> Result<DatabaseKey, SecretError> {
        if bytes.len() != DATABASE_KEY_LEN {
            return Err(SecretError::Corrupt {
                what: "database_encryption_key has the wrong length",
            });
        }
        if bytes.iter().all(|&byte| byte == 0) {
            return Err(SecretError::Corrupt {
                what: "database_encryption_key is all-zero",
            });
        }
        Ok(DatabaseKey(bytes.to_vec()))
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
    /// The backing store returned material that is not what GramDrive stored
    /// — truncated, empty, or zeroed.
    ///
    /// Distinct from [`SecretError::NotFound`]: the item exists but is
    /// unusable, and there is deliberately no fallback to a default or empty
    /// key. Raised by [`DatabaseKey::from_stored`].
    Corrupt {
        /// Which item was malformed and how — a fixed label, never a secret.
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
            SecretError::Corrupt { what } => write!(f, "secret corrupt: {what}"),
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

/// The write half of the platform secret store: the key lifecycle.
///
/// [`SecretSource`] reads secrets to configure a client; `SecretStore` owns
/// their lifecycle — creation on first authorization, rotation, and deletion
/// on logout. The native adapter implements both over the OS keychain
/// (SEC-002); the core ships only the seam and the in-memory double, so no
/// keychain API reaches a core crate.
///
/// The store is shared, externally-mutable state (the OS keychain), so every
/// method takes `&self`, and the trait is `Send + Sync` for use behind an
/// `Arc` across the runtime and, eventually, the FFI boundary.
///
/// # Lifecycle
///
/// - **Create** — on first authorization, draw a fresh key with
///   [`DatabaseKey::from_entropy`] and persist it with
///   [`put_database_key`](SecretStore::put_database_key). Retrieval is then
///   [`SecretSource::database_key`].
/// - **Rotate** — generate a new key, submit `setDatabaseEncryptionKey`
///   ([`super::set_database_encryption_key_request`]) to the running client,
///   and persist the new key with `put_database_key` *only* after TDLib
///   answers `ok`. Persisting before TDLib accepts the new key would strand
///   the database under a key the keychain no longer holds.
/// - **Delete** — on logout, [`delete_account`](SecretStore::delete_account)
///   drops the account's key: the keychain half of the SEC-004 cleanup whose
///   on-disk half is [`super::StorageLayout::wipe_account`]. The product
///   `api_id`/`api_hash` are app-lifetime and shared across accounts, so they
///   are *not* dropped per-account — only on a full app reset.
///
/// # Recovery
///
/// A key that comes back [`SecretError::NotFound`] or [`SecretError::Corrupt`]
/// is unrecoverable — the encrypted database it opened can no longer be read.
/// Recovery is therefore re-initialization, never a plaintext fallback: wipe
/// the account's on-disk state with [`super::StorageLayout::wipe_account`],
/// create a fresh key, and re-authorize. The typed error is what lets the
/// caller distinguish this from a transient backend failure.
pub trait SecretStore: SecretSource + Send + Sync {
    /// Persists `key` as `account`'s database key, creating it or replacing
    /// an existing one. Storing the same key twice leaves the same state.
    fn put_database_key(&self, account: AccountId, key: DatabaseKey) -> Result<(), SecretError>;

    /// Removes `account`'s database key. Deleting an absent account is
    /// success, not an error: logout must converge whether or not a key was
    /// ever stored (mirrors [`super::StorageLayout::wipe_account`]).
    fn delete_account(&self, account: AccountId) -> Result<(), SecretError>;
}

/// An in-process [`SecretSource`] and [`SecretStore`] for tests and CI.
///
/// Production resolves secrets from the platform keychain; this holds them
/// in memory instead (CI injects them from repository secrets, tests from
/// fixtures). It is a legitimate product type, not test-only code: it keeps
/// the artifact-free build paths — every gate that never touches a keychain
/// — able to exercise the full configuration and key-lifecycle flow.
///
/// The key map is behind a [`Mutex`] because [`SecretStore`] mutates through
/// `&self`, matching the shared-mutable shape of a real keychain; a poisoned
/// lock is recovered rather than propagated, since the only state it guards
/// is the secret map itself and no invariant spans a panic.
#[derive(Debug)]
pub struct InMemorySecrets {
    api: ApiCredentials,
    keys: Mutex<HashMap<i64, DatabaseKey>>,
}

impl InMemorySecrets {
    /// A source carrying only the product credentials; per-account keys are
    /// added with [`InMemorySecrets::with_key`] or [`SecretStore::put_database_key`].
    pub fn new(api: ApiCredentials) -> InMemorySecrets {
        InMemorySecrets {
            api,
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// Registers the database key for `account`, replacing any prior one.
    /// The builder form of [`SecretStore::put_database_key`], for fixtures.
    pub fn with_key(self, account: AccountId, key: DatabaseKey) -> InMemorySecrets {
        self.lock_keys().insert(account.0, key);
        self
    }

    /// The key map guard, recovering the inner data if a prior holder
    /// panicked. Avoids `unwrap`/`expect`, which the core denies.
    fn lock_keys(&self) -> std::sync::MutexGuard<'_, HashMap<i64, DatabaseKey>> {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretSource for InMemorySecrets {
    fn api_credentials(&self) -> Result<ApiCredentials, SecretError> {
        Ok(self.api.clone())
    }

    fn database_key(&self, account: AccountId) -> Result<DatabaseKey, SecretError> {
        self.lock_keys()
            .get(&account.0)
            .cloned()
            .ok_or(SecretError::NotFound {
                what: "database_encryption_key",
            })
    }
}

impl SecretStore for InMemorySecrets {
    fn put_database_key(&self, account: AccountId, key: DatabaseKey) -> Result<(), SecretError> {
        self.lock_keys().insert(account.0, key);
        Ok(())
    }

    fn delete_account(&self, account: AccountId) -> Result<(), SecretError> {
        self.lock_keys().remove(&account.0);
        Ok(())
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

    #[test]
    fn from_entropy_produces_a_full_length_key() {
        let key = DatabaseKey::from_entropy([0xab; DATABASE_KEY_LEN]);
        // 32 bytes base64-encode to 44 chars (ceil(32/3)*4), no padding gap.
        assert_eq!(key.base64().len(), DATABASE_KEY_LEN.div_ceil(3) * 4);
        // A generated key round-trips through the validated read boundary.
        assert!(DatabaseKey::from_stored(&[0xab; DATABASE_KEY_LEN]).is_ok());
    }

    #[test]
    fn from_stored_accepts_a_well_formed_key() {
        let mut bytes = [0u8; DATABASE_KEY_LEN];
        bytes[0] = 1; // not all-zero
        let key = DatabaseKey::from_stored(&bytes).expect("well-formed key");
        // The bytes survive intact: base64 of the same input matches.
        assert_eq!(
            key.base64(),
            DatabaseKey::from_bytes(bytes.to_vec()).base64()
        );
    }

    #[test]
    fn from_stored_rejects_wrong_length_with_no_fallback() {
        // Empty is the dangerous case: TDLib reads an empty key as "no
        // encryption", so it must fail closed, not fall back.
        assert!(matches!(
            DatabaseKey::from_stored(&[]),
            Err(SecretError::Corrupt { .. })
        ));
        // Truncated is corrupt too.
        assert!(matches!(
            DatabaseKey::from_stored(&[1u8; DATABASE_KEY_LEN - 1]),
            Err(SecretError::Corrupt { .. })
        ));
        // Over-length as well — only the exact size is a key.
        assert!(matches!(
            DatabaseKey::from_stored(&[1u8; DATABASE_KEY_LEN + 1]),
            Err(SecretError::Corrupt { .. })
        ));
    }

    #[test]
    fn from_stored_rejects_all_zero_key() {
        assert!(matches!(
            DatabaseKey::from_stored(&[0u8; DATABASE_KEY_LEN]),
            Err(SecretError::Corrupt { .. })
        ));
    }

    #[test]
    fn corrupt_error_display_names_the_item_without_a_secret() {
        let err = DatabaseKey::from_stored(&[0u8; DATABASE_KEY_LEN]).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.starts_with("secret corrupt:"));
        assert!(rendered.contains("all-zero"));
    }

    #[test]
    fn secret_store_put_get_delete_is_a_lifecycle() {
        let creds = ApiCredentials {
            api_id: 1,
            api_hash: Secret::new("h"),
        };
        let account = AccountId(42);
        let store = InMemorySecrets::new(creds);

        // Create: absent until a key is put.
        assert!(store.database_key(account).is_err());
        store
            .put_database_key(account, DatabaseKey::from_entropy([7; DATABASE_KEY_LEN]))
            .unwrap();
        assert_eq!(
            store.database_key(account).unwrap().base64(),
            DatabaseKey::from_entropy([7; DATABASE_KEY_LEN]).base64()
        );

        // Rotate: put replaces in place.
        store
            .put_database_key(account, DatabaseKey::from_entropy([9; DATABASE_KEY_LEN]))
            .unwrap();
        assert_eq!(
            store.database_key(account).unwrap().base64(),
            DatabaseKey::from_entropy([9; DATABASE_KEY_LEN]).base64()
        );

        // Delete: gone, and deleting again is idempotent success.
        store.delete_account(account).unwrap();
        assert!(matches!(
            store.database_key(account),
            Err(SecretError::NotFound { .. })
        ));
        store.delete_account(account).unwrap();
    }

    #[test]
    fn secret_store_delete_isolates_accounts() {
        let creds = ApiCredentials {
            api_id: 1,
            api_hash: Secret::new("h"),
        };
        let kept = AccountId(1);
        let dropped = AccountId(2);
        let store = InMemorySecrets::new(creds);
        store
            .put_database_key(kept, DatabaseKey::from_entropy([1; DATABASE_KEY_LEN]))
            .unwrap();
        store
            .put_database_key(dropped, DatabaseKey::from_entropy([2; DATABASE_KEY_LEN]))
            .unwrap();

        store.delete_account(dropped).unwrap();

        // Dropping one account's key leaves the other's intact (isolation).
        assert!(store.database_key(kept).is_ok());
        assert!(matches!(
            store.database_key(dropped),
            Err(SecretError::NotFound { .. })
        ));
    }

    #[test]
    fn in_memory_secrets_debug_redacts_stored_keys() {
        let creds = ApiCredentials {
            api_id: 1,
            api_hash: Secret::new("h"),
        };
        let store = InMemorySecrets::new(creds).with_key(
            AccountId(1),
            DatabaseKey::from_bytes(b"DBKEY-leak-me".to_vec()),
        );
        let rendered = format!("{store:?}");
        assert!(
            !rendered.contains("DBKEY-leak-me"),
            "key leaked: {rendered}"
        );
        // The key is present in the map but rendered through its redacted Debug.
        assert!(
            rendered.contains("<redacted>"),
            "expected redaction: {rendered}"
        );
    }
}
