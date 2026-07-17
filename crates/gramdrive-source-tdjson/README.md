# gramdrive-source-tdjson

Local TDLib source crate: the safe runtime over TDLib's C JSON interface
(`td_json_client.h`), implemented by TASK-260715-2ulon7. This layer turns
tdjson's asynchrony — one process-global receive stream multiplexing every
client's responses and updates — into safe, correlated, cancellable Rust:
client lifecycle, `@extra` request correlation, ordered bounded update
dispatch, typed error conversion, and coordinated shutdown with a
deterministic drain. On top of it sit the configuration layer (`config`,
TASK-260715-1hdnuy) and the authorization state machine (`auth`,
TASK-260715-51n6jb); the `DriveSource` adapter over this runtime (DEC-003)
lands with the enumeration follow-ups of the owning stories.

## Ownership

STORY-260715-3elo6l (tdlib-runtime-integration), EPIC-260715-2ptb18
(local-tdlib-source).

## The runtime at a glance

| Module | Owns |
|---|---|
| `api` | The two-trait seam: `TdSendApi` (thread-safe: create/send/execute) and `TdReceiveApi` (single-owner: receive takes `&mut self`) |
| `auth` | `AuthMachine`: the deterministic sans-IO authorization state machine — typed core-facing states/inputs for the phone/code/2FA-password/QR paths, rejection classification with retry advice, unknown TDLib states failing safe as typed `Unsupported` |
| `config` | `TdlibConfig` and the storage/memory policy: per-account `setTdlibParameters`/`setOption`/`addProxy` request builders, on-disk isolation with a clean logout wipe (`StorageLayout`), and the `SecretSource` keychain seam |
| `removal` | `AccountRemoval`: the crash-resumable account-removal workflow (SEC-004) — the SEC-004 cleanup sequenced behind a durable journal, distinguishing Telegram logout (`RevokeSession` → `logOut`) from local-only removal (`LocalOnly` → `close`), idempotent per stage and fail-safe under concurrent access |
| `snapshot` | `SnapshotMachine`: the deterministic sans-IO initial chat-list snapshot (TASK-260715-30amrq) — `loadChats` pagination per list, the `getChats` order witness, lazy `getChat` detail resolution, flood-wait backoff advice, resumable per-list commits carrying Telegram's exact ordering metadata |
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

## The authorization state machine (`auth`)

`AuthMachine` (TASK-260715-51n6jb) turns TDLib's `updateAuthorizationState`
events and user actions into a deterministic, core-facing flow. It is
sans-IO: the caller — the coming `DriveSource` adapter, or a native shell
through the FFI boundary — pumps the client's `UpdateStream` into
`on_update`, submits the requests each step returns (the machine answers
`waitTdlibParameters` with `TdlibConfig::startup_requests()` itself), turns
user actions into requests through `on_input`, and classifies a failed
submission with `AuthRejection::classify`, which pairs every rejection with
typed `RetryAdvice`.

Guarantees the scripted flows pin (`tests/auth_flow.rs`):

- **TDLib's reported state is the single source of truth.** Inputs never
  move the typed state; a rejected code or password leaves the flow exactly
  where TDLib says it is, so retries need no special path and an
  interrupted sign-in resumes from whatever state TDLib reports first.
- **First-class paths.** Phone → code → optional 2FA password, and QR
  confirmation → optional 2FA password, as typed states carrying the
  display material (code info, password hint, QR link) and typed inputs
  (submit/resend/cancel).
- **Unknown states fail safe.** Email gates, registration, and any state a
  future TDLib adds become the typed `Unsupported` state: every input but
  `Cancel` fails with a typed error, nothing panics, and cancel still
  closes the client.
- **Cancel is local.** `Cancel` maps to `close` — abandoning the flow;
  server-side logout, revocation, and the storage wipe are account
  removal's flow (TASK-260715-wjaux5, SEC-004).
- **Credentials stay redacted.** The login code and 2FA password ride in
  `Secret` (SEC-020): plaintext reaches only the wire request to TDLib.

## The account-removal workflow (`removal`)

`AccountRemoval` (TASK-260715-wjaux5) sequences the SEC-004 cleanup for one
account as a crash-resumable, idempotent workflow: quiesce transfers →
terminate the session → wipe the on-disk database and cached exports →
revoke the keychain key → purge the state rows. It is a driver loop — read
`next_pending`, perform the stage's effect, durably `complete` it — behind a
journal written outside the account's own subtree so the wipe cannot delete
its own progress record; `finalize` removes the journal last, leaving no
trace.

Layering keeps this crate honest: two stages act on crates above it —
cancelling transfers/unregistering provider state (`gramdrive-engine`) and
purging state rows (`gramdrive-state`) — so `SignalQuiesce` and `PurgeState`
are typed directives the composing caller executes, while the stages this
crate owns (the session request, the on-disk wipe, keychain revocation, the
journal) it runs directly.

Guarantees the fixtures pin (`tests/account_removal.rs`):

- **Telegram logout versus local-only removal is explicit.** `RevokeSession`
  submits `logOut` (Telegram terminates this authorization server-side);
  `LocalOnly` submits `close` (the server session is left intact, only local
  state is torn down). Everything after the session step is identical.
- **A full removal leaves no trace.** After the workflow finishes, a
  recursive scan of the storage root finds nothing referencing the account —
  subtree, exports, and journal all gone — while sibling accounts are
  untouched (per-account isolation).
- **Partial failure resumes.** Every stage is idempotent (a missing
  directory, an absent key, an already-closed client are all success), so a
  crash after an effect but before its record simply re-runs the effect on
  resume; `AccountRemoval::pending` returns every in-progress removal to
  finish on restart.
- **Concurrent access fails safe.** `AccountRemoval::guard_open` refuses
  (`RemovalError::InProgress`) while a removal is in flight, so a concurrent
  open never observes a half-wiped account; a second `begin` adopts the
  in-progress removal rather than racing it.
- **Cached exports are retained or discarded by explicit choice.**
  `ExportPolicy::Retain` omits the export-wipe stage entirely; `Discard`
  removes the registered export directories with everything else.

## The initial chat-list snapshot (`snapshot`)

`SnapshotMachine` (TASK-260715-30amrq) turns TDLib's chat-list loading
protocol into deterministic, resumable, per-list commits — the metadata
baseline everything else (history crawl, live updates, folder sync) builds
on. Sans-IO like the auth machine: the caller submits the requests
`next_step` names, pumps the client's updates into `on_update`, feeds
response outcomes to `on_response`, and persists each `ListCommit`
atomically — canonical chat rows, ordered membership, and the commit's
resume token in one state transaction (SYNC-022), with
`SNAPSHOT_CURSOR_STREAM` as the cursor convention.

Guarantees the suites pin (`tests/chat_snapshot.rs`, unit tests in
`src/snapshot.rs`):

- **Metadata only (SYNC-020).** The entire request surface is `loadChats`
  (pagination; the end-of-list `404` is the terminator, not a failure),
  `getChats` (the exact-order witness), and lazy `getChat` for chats the
  witness names that updates did not announce. No history, no media, no
  per-chat user/supergroup fan-out — usernames ride the
  `updateUser`/`updateSupergroup` objects TDLib pushes during the load.
- **Exact ordering metadata (DEC-013/POL-1).** Every list entry carries
  Telegram's opaque int64 `order` (parsed from tdjson's string wire shape,
  exact at int64 range) and the pinned flag; entries are emitted
  pinned-first, order descending, id descending — byte-for-byte the order
  the state layer's `chat_list` read reproduces.
- **No duplicates, no gaps (SYNC-003).** A duplicate id in the order
  witness and a listed chat that even lazy resolution cannot place are
  typed contract failures; a chat that demonstrably left the list mid-load
  (explicit order-0 position) is excluded and counted, never resurrected
  and never a false gap. Secret chats (POL-4) and unknown chat types are
  excluded and counted, fail-safe.
- **Resume without rework it cannot avoid (SYNC-004/SYNC-022).** Progress
  is list-granular because `loadChats` has no offset — TDLib's local
  database is the page cache, so an interrupted list re-enumerates locally
  rather than re-downloading. A resumed machine skips committed lists
  entirely; re-running a list is idempotent (upserts plus atomic membership
  replace), so interruption at any point yields neither duplicates nor
  gaps. Resume tokens are versioned and rejected explicitly when
  unreadable, never treated as an empty history.
- **Flood wait is advice, not failure (SYNC-044).** Codes 429/`FLOOD_WAIT`
  (with Telegram's stated delay parsed out) and 500 arm one typed
  `Backoff` step and re-issue the identical request; everything else is a
  typed terminal error whose recovery path is the durable token.

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
isolation and secret lookup on `AccountId` (DOM-020/DOM-021), and the
`snapshot` layer speaks `ChatListKind` for the lists it enumerates;
`gramdrive-source` remains reserved for the coming `DriveSource` adapter.
External: `serde_json` — JSON is tdjson's wire format and the config
request type. Dev-only: `gramdrive-state` — the snapshot integration suite
applies commits through the real typed repositories; product code never
links it from here (the composing caller owns that wiring). Platform-
specific code: forbidden — the keychain lives behind the `SecretSource`
seam, implemented in the native adapter. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-source-tdjson
```

Real-linkage smoke against the staged artifact (builds it first if needed
via `make tdlib`):

```sh
make tdjson-smoke
```
