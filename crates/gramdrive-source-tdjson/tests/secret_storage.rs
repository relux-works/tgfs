//! Session-secret storage lifecycle fixtures (TASK-260715-2odowl): the
//! database encryption key is created, retrieved, rotated, and deleted
//! through the [`SecretStore`] seam; a missing or corrupt key fails closed
//! with a typed error and never falls back to plaintext; per-account keys
//! stay isolated across a logout; and the on-disk and keychain halves of the
//! SEC-004 logout compose into a clean, recoverable wipe.
//!
//! The actual macOS Keychain binding is native-adapter code (DEC-002,
//! `crates/README.md` platform ban); these fixtures exercise the seam and its
//! contract against the artifact-free [`InMemorySecrets`], which is exactly
//! what the native adapter must reproduce over the OS keychain.

// clippy.toml exempts test code; restated for this integration binary,
// matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_model::identity::AccountId;
use gramdrive_source_tdjson::config::{
    AccountConfig, ApiCredentials, DATABASE_KEY_LEN, DatabaseKey, InMemorySecrets, Secret,
    SecretError, SecretSource, SecretStore, StorageLayout, set_database_encryption_key_request,
};

const API_HASH_SENTINEL: &str = "APIHASH-leak-me";

/// A unique-per-call temp directory, using the process id and a counter so
/// parallel test binaries cannot collide (no clock, no randomness).
fn temp_root() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "gramdrive-secret-storage-test-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn empty_store() -> InMemorySecrets {
    InMemorySecrets::new(ApiCredentials {
        api_id: 424242,
        api_hash: Secret::new(API_HASH_SENTINEL),
    })
}

/// A distinct 32-byte key seeded off `tag`, so different accounts get
/// different, well-formed keys without any randomness in the test.
fn key(tag: u8) -> DatabaseKey {
    DatabaseKey::from_entropy([tag; DATABASE_KEY_LEN])
}

/// The public base64 of a key, as it appears on the TDLib wire. `base64()`
/// is crate-private, so the rotation request — whose `new_encryption_key` is
/// that same encoding — is the oracle a downstream crate has for comparing a
/// key against a `setTdlibParameters.database_encryption_key` value.
fn wire_key(k: &DatabaseKey) -> serde_json::Value {
    set_database_encryption_key_request(k)["new_encryption_key"].clone()
}

#[test]
fn create_then_retrieve_resolves_a_wire_ready_config() {
    let store = empty_store();
    let account = AccountId(7);
    let layout = StorageLayout::new("/root");

    // Create on first authorization: the native adapter would draw entropy
    // from the OS CSPRNG; here from_entropy stands in.
    store.put_database_key(account, key(0x11)).unwrap();

    // Retrieve through resolve: the config carries the stored key on the wire.
    let config = AccountConfig::mirror(account, &layout)
        .resolve(&store)
        .expect("resolve succeeds once the key exists");
    let request = config.set_parameters_request();
    assert_eq!(
        request["database_encryption_key"],
        wire_key(&key(0x11)),
        "the resolved config carries exactly the created key"
    );
}

#[test]
fn missing_key_resolves_to_a_typed_error_not_a_fallback() {
    let store = empty_store();
    let layout = StorageLayout::new("/root");

    // No key was ever stored: resolve must fail closed, never fabricate one.
    let err = AccountConfig::mirror(AccountId(7), &layout)
        .resolve(&store)
        .expect_err("no key means no config");
    assert!(matches!(
        err,
        SecretError::NotFound {
            what: "database_encryption_key"
        }
    ));
}

/// A source whose keychain item came back malformed — the shape a truncated
/// or blanked OS keychain entry takes when read through
/// [`DatabaseKey::from_stored`].
struct CorruptKeySource(InMemorySecrets);

impl SecretSource for CorruptKeySource {
    fn api_credentials(&self) -> Result<ApiCredentials, SecretError> {
        self.0.api_credentials()
    }

    fn database_key(&self, _account: AccountId) -> Result<DatabaseKey, SecretError> {
        // An all-zero item is corrupt; from_stored rejects it, and that
        // rejection is what the caller sees — no usable key is produced.
        DatabaseKey::from_stored(&[0u8; DATABASE_KEY_LEN])
    }
}

#[test]
fn corrupt_key_resolves_to_a_typed_error_not_a_fallback() {
    let source = CorruptKeySource(empty_store());
    let layout = StorageLayout::new("/root");

    let err = AccountConfig::mirror(AccountId(7), &layout)
        .resolve(&source)
        .expect_err("a corrupt key must not resolve");
    assert!(
        matches!(err, SecretError::Corrupt { .. }),
        "corrupt key surfaces as a typed Corrupt error, not an empty-key fallback"
    );
}

#[test]
fn rotation_replaces_the_key_and_the_request_carries_the_new_one() {
    let store = empty_store();
    let account = AccountId(7);
    let layout = StorageLayout::new("/root");

    store.put_database_key(account, key(0x11)).unwrap();
    let old_wire = AccountConfig::mirror(account, &layout)
        .resolve(&store)
        .unwrap()
        .set_parameters_request()["database_encryption_key"]
        .clone();

    // Rotate: the live client is told first, then the keychain is updated
    // only on success (here we drive both steps directly).
    let new_key = key(0x22);
    let rotate = set_database_encryption_key_request(&new_key);
    assert_eq!(rotate["@type"], "setDatabaseEncryptionKey");
    store.put_database_key(account, new_key).unwrap();

    let new_wire = AccountConfig::mirror(account, &layout)
        .resolve(&store)
        .unwrap()
        .set_parameters_request()["database_encryption_key"]
        .clone();
    assert_ne!(old_wire, new_wire, "rotation changed the on-wire key");
    // The rotation request and the reopened config agree on the new key.
    assert_eq!(new_wire, wire_key(&key(0x22)));
    assert_eq!(rotate["new_encryption_key"], new_wire);
}

#[test]
fn logout_deletes_both_the_on_disk_state_and_the_keychain_key() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    let bystander = AccountId(8);

    // Materialize both accounts: keys in the store, files on disk.
    for (account, tag) in [(victim, 0x11u8), (bystander, 0x22u8)] {
        store.put_database_key(account, key(tag)).unwrap();
        let paths = layout.account_paths(account);
        std::fs::create_dir_all(paths.database_directory()).unwrap();
        std::fs::write(paths.database_directory().join("db.binlog"), b"state").unwrap();
    }

    // SEC-004 logout of the victim: keychain half then on-disk half.
    store.delete_account(victim).unwrap();
    layout.wipe_account(victim).expect("on-disk wipe succeeds");

    // The victim is gone from both stores...
    assert!(matches!(
        store.database_key(victim),
        Err(SecretError::NotFound { .. })
    ));
    assert!(!layout.account_dir(victim).exists());

    // ...and the bystander is untouched in both (per-account isolation).
    assert!(store.database_key(bystander).is_ok());
    assert!(layout.account_dir(bystander).exists());

    // Both halves are idempotent: logout must converge on a repeat.
    store.delete_account(victim).unwrap();
    layout
        .wipe_account(victim)
        .expect("second wipe is idempotent");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn recovery_after_an_unreadable_key_reinitializes_the_account() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let account = AccountId(7);

    // Stale on-disk database from a previous key that is now missing/corrupt.
    let paths = layout.account_paths(account);
    std::fs::create_dir_all(paths.database_directory()).unwrap();
    std::fs::write(paths.database_directory().join("db.binlog"), b"stale").unwrap();

    // Recovery is re-initialization, never a plaintext fallback: wipe the
    // unreadable on-disk state, create a fresh key, and re-authorize.
    layout.wipe_account(account).expect("wipe unreadable state");
    store.put_database_key(account, key(0x33)).unwrap();

    let config = AccountConfig::mirror(account, &layout)
        .resolve(&store)
        .expect("resolve succeeds after recovery");
    assert_eq!(
        config.set_parameters_request()["database_encryption_key"],
        wire_key(&key(0x33))
    );

    std::fs::remove_dir_all(&root).ok();
}
