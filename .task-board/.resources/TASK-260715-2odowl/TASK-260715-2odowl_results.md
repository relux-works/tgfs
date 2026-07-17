# TASK-260715-2odowl — Session encryption & secure key references

**Status:** ready for review
**Scope:** database-encryption-key lifecycle seam in `gramdrive-source-tdjson::config`
**Gates:** `make check` → 8/8 green (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts)

## What this task owns (and what it deliberately does not)

The Rust core owns the **key-management contract**: the seam through which the
platform secure store creates, retrieves, rotates, and deletes the TDLib
database encryption key and product credentials, plus the safe-failure
semantics around it.

The **actual macOS Keychain binding is native-adapter code**, not core code.
This is the established architecture, not a gap or a forced fit:

- `crates/README.md` (Feature and platform policy): *core crates contain no
  platform-specific code* — no `target_os` cfg, no `core-foundation`/Security
  dependency — and *secure-store APIs live in the native adapter layers*. The
  ban is enforced fail-closed by `.scripts/check_crate_architecture.py`.
- `DEC-002` (Accepted): keep UI and provider entry points native (Swift /
  Kotlin / WinUI).
- LOGBOOK decision (2026-07-17): *keychain is a seam, not code in core* —
  `SecretSource` resolves creds + per-account key at runtime; the native
  adapter implements it over the OS keychain; core ships only the trait +
  `InMemorySecrets`.

So a Rust `#[cfg(target_os = "macos")]` Security-framework implementation would
have failed the architecture gate and inverted the layering. The macOS Keychain
implementation is deferred to the native application layer (no Swift app exists
in this workspace yet). `InMemorySecrets` is the artifact-free reference the
native adapter must reproduce over the OS keychain.

## Delivered — the lifecycle seam

`crates/gramdrive-source-tdjson/src/config/secret.rs`:

- **`DATABASE_KEY_LEN = 32`** — 256-bit key, the only size we create.
- **`DatabaseKey::from_entropy([u8; 32])`** — *creation*. The native adapter
  draws 32 bytes from the OS CSPRNG (`SecRandomCopyBytes` on macOS) and calls
  this on first authorization. The CSPRNG stays on the platform side — the
  core links no `rand`/`getrandom` — and the fixed-size array makes a
  wrong-length created key unrepresentable.
- **`DatabaseKey::from_stored(&[u8]) -> Result<_, SecretError>`** — *validated
  retrieval boundary*. Rejects a wrong-length (incl. empty) or all-zero item
  with `SecretError::Corrupt`. This is the "no plaintext fallback" guarantee:
  TDLib treats an **empty** `database_encryption_key` as *database not
  encrypted*, so a truncated/blanked keychain item must fail closed instead of
  silently opening plaintext.
- **`SecretError::Corrupt { what }`** — new typed variant, distinct from
  `NotFound`; carries only a fixed label, never key bytes.
- **`SecretStore: SecretSource + Send + Sync`** — the write half of the seam:
  - `put_database_key(account, key)` — create or replace (rotation).
  - `delete_account(account)` — the keychain half of the SEC-004 logout;
    idempotent, mirrors `StorageLayout::wipe_account`.
  - `&self` methods (shared external store); `Send + Sync` for `Arc` use across
    the runtime and, eventually, the FFI boundary — matching the foreign-trait
    shape (`ProgressListener`).
- **`InMemorySecrets`** now implements `SecretStore` too (Mutex-backed key map,
  poison-recovering lock so the core's `unwrap`/`expect` ban holds).

`crates/gramdrive-source-tdjson/src/config.rs`:

- **`set_database_encryption_key_request(&DatabaseKey)`** — the
  `setDatabaseEncryptionKey` request builder (`new_encryption_key`, base64),
  the executable form of the rotation path.

## Rotation path (documented)

1. Generate a new key: `DatabaseKey::from_entropy(os_csprng_32())`.
2. Submit `set_database_encryption_key_request(&new_key)` to the **running**
   client (TDLib re-encrypts the DB under the new key).
3. **Only on TDLib `ok`**, persist with `SecretStore::put_database_key`,
   replacing the old key. Persisting before TDLib accepts would strand the
   database under a key the keychain no longer holds.

## Recovery (documented)

A `NotFound` or `Corrupt` key is unrecoverable — the encrypted DB it opened
can't be read. Recovery is re-initialization, never a plaintext fallback:
`StorageLayout::wipe_account` the on-disk state → create a fresh key → re-auth.
The typed error is what lets the caller distinguish this from a transient
backend failure. Covered by `recovery_after_an_unreadable_key_reinitializes_the_account`.

## Logout deletion (SEC-004, this task's half)

- On-disk half: `StorageLayout::wipe_account` (pre-existing).
- Keychain half: `SecretStore::delete_account` (new). Drops the account's DB
  key only; product `api_id`/`api_hash` are shared + app-lifetime, dropped on
  full app reset, not per-account logout. Server-side logout/revocation is
  TASK-260715-wjaux5 (account removal), which this task unblocks.

## Per-account isolation

Keys are keyed on `AccountId` (`i64`, DOM-020/021); on-disk state on disjoint
`account-<id>` subtrees. Deleting one account's key/subtree leaves siblings
intact — proven by `secret_store_delete_isolates_accounts` (unit) and
`logout_deletes_both_the_on_disk_state_and_the_keychain_key` (integration).

## Tests

Unit (`src/config/secret.rs`, `src/config.rs`) + integration
(`tests/secret_storage.rs`):

- key roundtrip (create → retrieve → wire), `from_entropy` length
- missing key → `NotFound`, no fallback
- corrupt key (empty / truncated / over-length / all-zero) → `Corrupt`, and
  propagation through `AccountConfig::resolve` (no empty-key fallback)
- rotation request shape + key replacement
- logout deletes both halves, idempotent, isolates siblings
- recovery after unreadable key
- no secrets in logs: `Corrupt` Display carries no bytes; `InMemorySecrets`
  Debug redacts stored keys (existing `no_secret_reaches_any_debug_or_log_form`
  still green)

Counts: 47 lib unit tests (crate), 6 new integration tests in
`tests/secret_storage.rs`, all green.

## Files touched

- `src/config/secret.rs` — const, `from_entropy`/`from_stored`, `Corrupt`,
  `SecretStore`, `InMemorySecrets` store impl, unit tests
- `src/config.rs` — rotation request builder, re-exports, module doc, test
- `src/config/storage.rs` — logout doc now points at `SecretStore::delete_account`
- `src/lib.rs` — module doc + re-exports (`SecretStore`, `DATABASE_KEY_LEN`,
  `set_database_encryption_key_request`)
- `tests/secret_storage.rs` — new integration suite
