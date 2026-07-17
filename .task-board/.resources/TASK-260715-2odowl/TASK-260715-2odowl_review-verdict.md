# TASK-260715-2odowl — Review verdict: ACCEPTED

**Reviewer:** reviewer (claude) · **Date:** 2026-07-18 · **Verdict:** accepted → done

## Summary
Delivers the database-encryption-key lifecycle seam in the platform-neutral
core crate `gramdrive-source-tdjson::config`: creation (`DatabaseKey::from_entropy`),
validated retrieval (`from_stored` → `SecretError::Corrupt`, no plaintext fallback),
rotation (`set_database_encryption_key_request` + `SecretStore::put_database_key`,
persist-on-ok), and logout deletion (`SecretStore::delete_account`). Clean,
idiomatic, exceptionally well-documented.

## Acceptance criteria — all met (independently verified)
- **No secret in logs/config/git.** `DatabaseKey`/`ApiCredentials`/`Secret` have
  manual redacting `Debug`; `SecretError` carries only fixed `&'static str`
  labels, never bytes. Test fixtures use fake sentinels ("DBKEY-leak-me"),
  no real secrets. Tested (`in_memory_secrets_debug_redacts_stored_keys`,
  `corrupt_error_display_names_the_item_without_a_secret`).
- **Missing/corrupt fails safely.** `NotFound` vs `Corrupt` typed errors,
  propagated through `AccountConfig::resolve`; no empty-key fallback (TDLib
  reads an empty key as "unencrypted", so wrong-length incl. empty + all-zero
  are rejected). Tested at unit and integration level.
- **Cleanup + multi-account isolation.** `delete_account` (keychain half) +
  `wipe_account` (on-disk half) compose the SEC-004 logout; idempotent;
  siblings untouched. Tested (`secret_store_delete_isolates_accounts`,
  `logout_deletes_both_the_on_disk_state_and_the_keychain_key`).

## Architecture fit — confirmed
`gramdrive-source-tdjson` is in `CORE_CRATES`/`PLATFORM_NEUTRAL_CRATES`
(`.scripts/check_crate_architecture.py`); a `cfg(target_os="macos")`
Security-framework impl would FAIL the architecture gate. Shipping the seam +
`InMemorySecrets` (Mutex-backed, poison-recovering, `unwrap`/`expect`-ban safe)
is the correct core contribution. Consistent with DEC-002, `crates/README.md`
platform ban, `.spec/architecture.md`, and SEC-002/003/004.

## macOS Keychain impl deferral — legitimate, not a forced fit
The DoD's "macOS Keychain impl" is native-adapter scope. No native/Swift layer
exists anywhere in the workspace (EPIC-260715-2ptb18 is entirely Rust core), so
the binding has no home here yet. Deferral is honestly documented (logbook
2026-07-18, results artifact) and architecturally mandated. Forcing keychain
code into this crate would BE the forced fit.

## Gates — verified independently
`make check` → 8/8 green (toolchain, format, lint, test, architecture,
supply-chain, traceability, scripts). `cargo test -p gramdrive-source-tdjson`
→ 8 lib + 6 `secret_storage.rs` integration + runtime suites, all green.

## Minor non-blocking observations (future hardening, not rework)
1. `from_entropy` accepts an all-zero array while `from_stored` rejects it —
   a tiny asymmetry; negligible (a broken CSPRNG is catastrophic anyway).
   Optional defense-in-depth: reject all-zero in `from_entropy` too.
2. `from_bytes` is a `pub` unvalidated constructor; the validation boundary
   is only enforced when callers use `from_stored`. Only test fixtures use it
   today; could be tightened if an external constructor is ever unneeded.
3. No zeroize-on-drop for key material (`Vec<u8>`). Beyond AC scope and
   consistent with existing `Secret` handling; candidate for a later hardening
   task if the threat model warrants it.

None of these warrant a rework cycle. Accepted.
