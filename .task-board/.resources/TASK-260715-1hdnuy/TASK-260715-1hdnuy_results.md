# TASK-260715-1hdnuy — Define TDLib configuration and storage policy

**Status:** ready for review (`to-review`)
**Story:** STORY-260715-3elo6l (tdlib-runtime-integration)
**Epic:** EPIC-260715-2ptb18 (local-tdlib-source)

## What was built

A `config` module in the `gramdrive-source-tdjson` crate that turns an
account's identity + storage layout + platform secrets into the ordered
TDLib initialization request sequence, and owns the storage/memory policy
those requests encode. No product code sends the requests yet — that is the
authorization task; this layer produces them and manages on-disk state.

### Files

- `crates/gramdrive-source-tdjson/src/config.rs` — public surface:
  `DeviceMetadata`, `StoragePolicy`, `MemoryOptions`, `Proxy`,
  `AccountConfig` (secret-free plan), `TdlibConfig` (resolved, wire-ready),
  request builders (`set_parameters_request`, `option_requests`,
  `proxy_request`, `startup_requests`).
- `crates/gramdrive-source-tdjson/src/config/secret.rs` — `Secret`,
  `DatabaseKey`, `ApiCredentials` (all redacting), `SecretError`,
  `SecretSource` trait (the keychain seam), `InMemorySecrets`, an in-crate
  RFC-4648 base64 encoder.
- `crates/gramdrive-source-tdjson/src/config/storage.rs` — `StorageLayout`,
  `AccountStoragePaths`, per-account isolation + `wipe_account`.
- `crates/gramdrive-source-tdjson/tests/config.rs` — 4 fixtures (log-scrub,
  upgrade, logout, runtime round-trip).
- `lib.rs` (+`pub mod config`, re-exports, module doc), `Cargo.toml`
  (+`gramdrive-model`), crate `README.md` (config section).

## Acceptance criteria → evidence

### Configuration minimizes memory per evidence

`setTdlibParameters` (`StoragePolicy::mirror`):

| Flag | Value | Why |
|---|---|---|
| `use_file_database` | true | file metadata/download state; required by message DB |
| `use_chat_info_database` | true | chat/user info; required by message DB |
| `use_message_database` | true | the mirror's local history source (`.spec/architecture.md`, LocalTdlibSource line 111 — "TDLib supplies ordered updates, local database, file state") |
| `use_secret_chats` | false | GramDrive never archives secret chats |

TDLib dependency rule (message DB ⇒ chat-info + file DB) is respected and
asserted by `StoragePolicy::is_coherent()`.

`setOption` (`MemoryOptions::minimal`) — the minimization layer:

| Option | Value | Why |
|---|---|---|
| `use_storage_optimizer` | false | GramDrive owns the media-cache quota + LRU (POL-2); TDLib deleting unpromoted files would corrupt that accounting |
| `ignore_background_updates` | false | mirror correctness needs every update — deliberately NOT ignored, and stated as such |
| `message_unload_delay` | 60s | unload messages from RAM promptly to bound resident memory |
| `disable_persistent_network_statistics` | true | network-stats DB unused → not written |
| `notification_group_count_max` | 0 | GramDrive surfaces no Telegram notifications → no notification-group state |

Memory rationale ties to the same TDLib per-client memory concerns
(td#2516, td#2807) that excluded TDLib-in-extension in `.spec/architecture.md`.

### Never logs secrets (SEC-020/SEC-023)

`Secret`, `DatabaseKey`, and `ApiCredentials` redact under `Debug`
(`Secret(<redacted>)` etc.); `TdlibConfig`'s manual `Debug` redacts
credentials + db key; `AccountConfig` (the loggable plan) holds no
never-logged material (its proxy's password is a `Secret`, so it redacts
too). Plaintext leaves only through crate-private `expose()`/`base64()`,
whose only callers are the JSON request builders that write to the TDLib
wire.

Fixture `no_secret_reaches_any_debug_or_log_form` asserts that none of the
`api_hash`, `api_id`, database key, or proxy password sentinels appear in
any `Debug` rendering — while the wire `setTdlibParameters` request *does*
carry them (proving the split is real, not that the value was dropped).

### Isolates accounts

`StorageLayout::account_dir` = `root/account-<id>`, injective over
`AccountId` (i64; negative ids valid and still injective). Database + files
directories live inside each account's subtree → disjoint by construction.
Asserted by `distinct_accounts_map_to_disjoint_subtrees` and the logout
fixture's bystander check.

### Survives upgrade / logout fixtures

- **Upgrade** (`storage_identity_survives_a_version_upgrade`): a version bump
  changes only `application_version`; every field deciding which encrypted
  database TDLib opens (`database_directory`, `files_directory`,
  `database_encryption_key`, `use_test_dc`, all DB flags, `api_id`,
  `api_hash`) is byte-identical → TDLib reopens the same store, no re-login.
- **Logout** (`logout_wipes_one_account_cleanly_and_leaves_siblings_intact`):
  real files materialized for two accounts; `wipe_account` removes exactly
  the victim's subtree, leaves the sibling intact, and is idempotent (second
  wipe is a no-op). On-disk half of the SEC-004 sequence; the keychain half
  (dropping the account's db key + product creds) is the native adapter's,
  done through `SecretSource`'s backing store.

### api_id/api_hash + db key from keychain abstraction, injected at runtime

`SecretSource` is the seam to platform secure storage (SEC-003). Native
adapter implements it over the OS keychain (macOS Keychain service
`gramdrive-telegram`, per TASK-260716-1iypv4); the core crate ships only the
trait + `InMemorySecrets` (tests + CI env-injection). No keychain/platform
code enters a core crate — the platform ban stays intact.

### Device/app metadata per ToS disclosure

`DeviceMetadata` (`device_model`, `system_version`, `application_version`,
`system_language_code`) sent in `setTdlibParameters` (SEC-030). `Default` is
a safe placeholder; the native adapter fills truthful values.

### Network/proxy options

`Proxy` = Direct (default, no request) | Socks5 | Http | Mtproto, emitting
`addProxy` with the credential redacted in `Debug`.

## Gates

`make check` → 8/8 green (`.temp/TASK-260715-1hdnuy/make-check-01.log`):
toolchain, format, lint (clippy `-D warnings`), test (`--workspace
--all-features`), architecture, supply-chain (`cargo deny`), traceability,
scripts.

Crate tests: 25 lib unit tests (13 new in `config`) + 4 integration
fixtures, all green, artifact-free (mock runtime). The startup sequence is
additionally round-tripped through the real `TdRuntime` over `MockTdJson` —
proving the built requests are well-formed JSON the runtime accepts,
correlates, and answers.

## Follow-ups / notes for review

- `setOption` names are validated here for JSON *shape* only (mock runtime).
  Real-linkage validation of each option name belongs to the `real_tdjson`
  smoke in a follow-up; a name TDLib does not recognize is a non-fatal error
  answer, not a crash.
- Consumers (auth flow) submit `TdlibConfig::startup_requests()` in order:
  `setTdlibParameters` first (TDLib requires it before most requests), then
  options, then optional `addProxy`.
- Registered Telegram app title is still the legacy `memori`
  (TASK-260716-1iypv4 note) — cosmetic, non-blocking.
