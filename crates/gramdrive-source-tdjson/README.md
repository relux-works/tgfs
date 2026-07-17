# gramdrive-source-tdjson

Local TDLib source crate: the safe runtime over TDLib's C JSON interface
(`td_json_client.h`), implemented by TASK-260715-2ulon7. This layer turns
tdjson's asynchrony — one process-global receive stream multiplexing every
client's responses and updates — into safe, correlated, cancellable Rust:
client lifecycle, `@extra` request correlation, ordered bounded update
dispatch, typed error conversion, and coordinated shutdown with a
deterministic drain. The `DriveSource` adapter over this runtime (DEC-003)
lands with the follow-up tasks of the owning story (configuration,
authorization, enumeration).

## Ownership

STORY-260715-3elo6l (tdlib-runtime-integration), EPIC-260715-2ptb18
(local-tdlib-source).

## The runtime at a glance

| Module | Owns |
|---|---|
| `api` | The two-trait seam: `TdSendApi` (thread-safe: create/send/execute) and `TdReceiveApi` (single-owner: receive takes `&mut self`) |
| `config` | `TdlibConfig` and the storage/memory policy: per-account `setTdlibParameters`/`setOption`/`addProxy` request builders, on-disk isolation with a clean logout wipe (`StorageLayout`), and the `SecretSource` keychain seam |
| `runtime` | `TdRuntime` (the one receive owner), `TdClient`, `PendingRequest` (blocking wait, `Future`, cancellation), `UpdateStream`, `RuntimeConfig`, `RuntimeStats` |
| `error` | `TdError`: typed conversion of `{"@type":"error"}` objects plus runtime lifecycle failures |
| `mock` | `MockTdJson`: the deterministic in-process tdjson double the tests run against |
| `real` | The FFI implementation over `libtdjson.dylib` — compiled only under `cfg(real_tdjson)` (below) |

## Semantics

**One receive owner.** `td_receive` must never be called concurrently; the
runtime moves the `TdReceiveApi` half into its single receive-loop thread,
and the real implementation additionally hands out its receiver exactly
once per process (`RealTdJson::claim`). Misuse is a compile error, not a
race.

**Correlation is runtime-owned.** Every request gets a minted JSON number
injected as `@extra`; a request that already carries `@extra` is rejected.
Responses resolve their `PendingRequest` — an `{"@type":"error"}` object as
the typed `TdError::Td`. A response with no pending entry (cancelled,
duplicate) is discarded and counted, never misdelivered.

**Cancellation is entry removal.** Dropping or cancelling a
`PendingRequest` removes the correlation entry immediately; tdjson has no
wire cancellation, so TDLib still finishes the work, but its answer can
only be counted (`RuntimeStats::discarded_responses`), never delivered to a
caller that gave up.

**Updates are ordered and bounded.** Events without `@extra` route by
`@client_id` into a bounded per-client queue. A full queue backpressures
the receive loop rather than dropping mid-stream (TDLib's update order is
contractual); the block is released by shutdown, by the consumer draining,
or by the consumer's stream being dropped. One slow consumer therefore
stalls the shared loop — a stated v1 tradeoff for the single-account shape.

**Shutdown drains, then fails the rest.** `TdRuntime::shutdown` (also on
drop) sets the flag, closes every update queue (waking a backpressured
loop — the reason shutdown cannot deadlock), joins the loop after it
processes every event tdjson already had ready, then fails still-pending
requests with `TdError::Shutdown`. Buffered updates stay readable; their
stream then reports closed.

**Close is destroy.** The modern tdjson interface has no destroy call:
a client ends when TDLib reports `authorizationStateClosed`, after which
the runtime fails that client's pending requests with
`TdError::ClientClosed`, ends its stream, and rejects new requests. The
deprecated `td_json_client_*` interface is linked (proved by the tdlib
link smoke) but never called — TDLib forbids mixing the two interfaces.

**No C pointer outlives its validity.** The `api` traits traffic only in
owned `String`s; the real implementation copies every tdjson-returned
C string before returning and keeps request `CString`s alive across the
call. Miri cannot execute FFI, so the full ownership justification lives in
`src/real.rs` module docs, and the same runtime logic runs under the mock
in every gate.

## Configuration and storage policy (`config`)

`AccountConfig::mirror(account, &layout)` builds the secret-free plan for an
account — its isolated storage paths, device/app metadata (SEC-030), the
`StoragePolicy` (file + chat-info + message databases on, secret chats off —
the mirror needs TDLib's local database as its history source), and the
`MemoryOptions` that minimize footprint (TDLib's own storage optimizer off
because GramDrive owns the cache quota and LRU per POL-2; prompt message
unload; no persistent network-statistics DB; no notification groups).

`plan.resolve(&secrets)` attaches the `api_id`/`api_hash` and the per-account
database encryption key from a `SecretSource` — the seam to platform secure
storage (macOS Keychain service `gramdrive-telegram`; the native adapter
implements the trait, no keychain code lives in this core crate). The
resulting `TdlibConfig::startup_requests()` is the ordered
`setTdlibParameters` → `setOption`s → optional `addProxy` sequence the
authorization flow submits.

Guarantees the fixtures pin (`tests/config.rs`):

- **Secrets never log.** `api_id`, `api_hash`, the database key, and proxy
  credentials are redacted from every `Debug`/log form; the plaintext
  reaches only the wire request to TDLib (SEC-020/SEC-023).
- **Isolation.** Distinct accounts map to disjoint on-disk subtrees, so one
  account's TDLib database can never touch another's.
- **Survives upgrade.** A version bump changes `application_version` only;
  every field that decides which encrypted database TDLib opens is
  byte-identical, so the upgrade reopens the same store.
- **Clean logout.** `StorageLayout::wipe_account` removes exactly one
  account's subtree, idempotently — the on-disk half of the SEC-004 logout
  sequence (the keychain half is the native adapter's).

## The env gate (`cfg(real_tdjson)`)

Default builds — every `make check`, on machines that never built the
TDLib artifact — compile mock-only: no linkage, no unsafe code
(`forbid(unsafe_code)` applies outside the gate). With
`GRAMDRIVE_TDLIB_ARTIFACT_DIR` pointing at the staged artifact
(`.temp/tdlib/out`, produced by `make tdlib`), `build.rs` enables
`cfg(real_tdjson)`, compiles `real`, links `libtdjson.dylib` and bakes in
its rpath.

Deliberately an environment gate rather than a cargo feature: the lint and
test gates run `--all-features`, so a feature would drag real linkage into
exactly the runs that must stay artifact-free (crates/README.md feature
policy).

## Dependencies

Internal: `gramdrive-model` — the `config` layer keys per-account storage
isolation and secret lookup on `AccountId` (DOM-020/DOM-021);
`gramdrive-source` remains reserved for the coming `DriveSource` adapter.
External: `serde_json` — JSON is tdjson's wire format and the config
request type. Platform-specific code: forbidden — the keychain lives behind
the `SecretSource` seam, implemented in the native adapter. See
`crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-source-tdjson
```

Real-linkage smoke against the staged artifact (builds it first if needed
via `make tdlib`):

```sh
make tdjson-smoke
```
