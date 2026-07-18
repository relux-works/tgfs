# gramdrive-source-tdjson

Local TDLib source crate: the safe runtime over TDLib's C JSON interface
(`td_json_client.h`), implemented by TASK-260715-2ulon7. This layer turns
tdjson's asynchrony — one process-global receive stream multiplexing every
client's responses and updates — into safe, correlated, cancellable Rust:
client lifecycle, `@extra` request correlation, ordered bounded update
dispatch, typed error conversion, and coordinated shutdown with a
deterministic drain. On top of it sit the configuration layer (`config`,
TASK-260715-1hdnuy) and the authorization state machine (`auth`,
TASK-260715-51n6jb). Of the `DriveSource` adapter over this runtime
(DEC-003), the ranged-read side is here (`download`, TASK-260715-1onbmf);
the enumeration side lands with the follow-ups of the owning stories.

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
| `history` | `CrawlMachine`: the deterministic sans-IO resumable per-chat history crawl (TASK-260715-26dnp6) — `getChatHistory` paging into normalized `MessageRecord`s, one commit per page carrying the durable `[oldest, newest]` window facts (`chat_sync_state`, SYNC-021/022), priority scheduling favoring visible/requested chats with page-by-page round-robin among equals, flood-wait backoff advice, and explicit per-chat unavailability for left/inaccessible chats |
| `live` | `LiveMachine`: the deterministic sans-IO ordered live message update loop (TASK-260715-10p5zp) — `updateNewMessage`/`updateMessageSendSucceeded`, edit signals resolved through a `getMessage` refresh, and permanent `updateDeleteMessages` folded into ordered per-chat commits over `normalize_message`, each carrying the cursor advance it justifies (`chat_sync_state` newest, merged by the caller, SYNC-022); a live message above an unverified committed window opens a targeted `getChatHistory` gap bridge, recovered before the cursor moves (SYNC-023), and a failed bridge freezes the cursor explicitly |
| `updates` | `UpdateMachine`: the deterministic sans-IO live chat-metadata/list update mapper (TASK-260715-1c8fea) — TDLib's push updates (title/photo/position/removed-from-list/protected-content, plus the user/supergroup username feed) folded into the same normalized change stream, with POL-1 invalidation classification (reorder → `order.json`, rename → folder rename), idempotent under duplicate and out-of-order delivery, and gap reporting for unknown chats |
| `folders` | `FolderCatalogMachine`: the deterministic sans-IO folder (chat filter) catalog reducer (TASK-260715-54nopz) — `updateChatFolders` folded into a normalized folder create/rename/delete/reorder change stream with POL-1 invalidation classification, yielding the ordered folder set the snapshot enumerates; folder membership stays the chat machines' appearances, so a folder deletion removes only appearances |
| `download` | `DownloadMachine` + `TdDownloader`: the ranged download adapter (TASK-260715-1onbmf) — the `DriveSource::fetch` side of this source. POL-4/version-pin/extent gates before any network call, synchronous ranged `downloadFile` with priority passthrough (1..=32), bounded local reads streamed into the caller's sink (never a whole file in memory, and TDLib's local file is read in place — never moved or deleted), per-file serialization (TDLib keeps one download conversation per file), `cancelDownloadFile` on abandon, and the `FILE_REFERENCE_*` → `getMessage` refresh surfacing as `StaleReference` with identity unmoved (SYNC-040..046, DOM-007). The `FetchCatalog` seam supplies per-item facts from the metadata store; conformance for ranged reads runs in `tests/fetch_conformance.rs` |
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

## Live chat-metadata/list updates (`updates`)

`UpdateMachine` (TASK-260715-1c8fea) keeps the snapshot baseline current from
TDLib's live push stream. It is a pure sans-IO reducer — no requests, no
client: feed every update to `on_update`, drain the accumulated normalized
changes with `take_batch`, and apply the `UpdateBatch` to state in one
transaction (canonical `chats` first for the `chat_list_entries → chats`
foreign key, memberships after). Membership deltas use the incremental
`upsert_chat_list_entry` / `remove_chat_list_entry` repo methods, so one chat's
move never rewrites the rest of a list.

Guarantees the suites pin (`tests/chat_updates.rs`, unit tests in
`src/updates.rs`):

- **The same normalized vocabulary as the snapshot.** `updateNewChat`,
  `updateChatTitle`, `updateChatPhoto`, `updateChatPosition`,
  `updateChatRemovedFromList`, `updateChatHasProtectedContent`, and the
  `updateUser`/`updateSupergroup` username feed fold into `ChatMetadata`
  upserts and `MembershipChange` deltas — one projection, kept current, never a
  second disagreeing one (SYNC-026).
- **Provider invalidation split (POL-1).** A reorder (position/pin change) is a
  content change: `Invalidation::ListOrdering` (regenerate `order.json`) and
  nothing else — it never rewrites the chat's canonical row, so identity is
  stable. A rename (title/username change of a known chat) emits
  `Invalidation::FolderName`. First sight, avatar, and protected-content
  changes emit `Invalidation::Metadata` (the metadata version advances; no
  folder or list order does).
- **Converge under duplicates, out-of-order, and restart.** Every field is
  applied last-write-wins; a value equal to the known one produces no output,
  so a duplicated or replayed update is a no-op. With a content-derived
  `metadata_version`, a restart fed TDLib's re-pushed burst rewrites nothing —
  the SYNC-003 enumeration anchor stays put.
- **Gaps, not forgeries (SYNC-003/023).** An update naming a chat with no known
  metadata cannot forge a canonical row (and the foreign key would reject its
  membership), so the value is dropped and the chat reported in
  `UpdateBatch::unresolved`; the caller resolves it with `getChat` (fed back as
  `updateNewChat`, carrying current title/avatar/positions) or re-baselines.
  There is deliberately no live resume token — the stream has no offset, and
  durability is idempotent convergence plus snapshot re-baselining. Secret and
  unknown chat types are excluded and never mistaken for gaps (POL-4/DEC-016).

## The folder (chat filter) catalog (`folders`)

`FolderCatalogMachine` (TASK-260715-54nopz) discovers the custom Telegram
folders that populate the "Telegram Folders/" catalog. It is a pure sans-IO
full-state reducer over one update — `updateChatFolders`, pushed on connect and
on every catalog change, always carrying the complete ordered folder list. Feed
each to `on_update`, drain the net change with `take_batch`, and apply the
`FolderCatalogBatch`. `folders()` yields the ordered folder-id set a composing
caller feeds into a `SnapshotPlan` (the snapshot machine snapshots the folders
it is given and left discovery here).

A folder is two separate facts: its *definition* (id, title, tab position) lives
here, while its *membership* is ordinary `chatListFolder` positions the snapshot
and update machines already fold into `chat_list_entries`. So a chat in several
folders is one canonical `chats` row with one appearance per list (DOM-022), and
deleting a folder clears only that folder's memberships — the caller applies
each `FolderCatalogBatch::removed` with an empty `replace_chat_list`, leaving the
canonical chats and every other list untouched (SYNC-026).

Guarantees the suites pin (`tests/folder_catalog.rs`, unit tests in
`src/folders.rs`):

- **Normalized create/rename/delete/reorder.** The batch is the diff between the
  last-observed catalog and the last-drained one, so a duplicate or replayed
  `updateChatFolders` is a no-op and a restart re-push converges without churn.
- **Provider invalidation split (POL-1/SYNC-011).** A rename (title change)
  emits `FolderInvalidation::Renamed`; a folder that only shifted tab position
  never does — a reorder is content, so it emits a single
  `FolderInvalidation::CatalogOrdering`. Creations and deletions emit `Created`
  and `Removed`; any set/order change adds one `CatalogOrdering`.
- **Membership is appearances, not canonical data.** The end-to-end suite drives
  both machines into a real store and reads back one canonical record with N
  appearances, a folder deletion that removes appearances only, and incremental
  folder create/rename/reorder that never disturbs a chat row.
- **Version-tolerant title parse.** The folder title is read across TDLib name
  shapes (`name.text.text`, `name.text`, bare `name`, legacy `title`); a folder
  entry without an id fails safe (skipped, never guessed).

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
`snapshot`, `updates`, and `folders` layers speak `ChatListKind`/`FolderId` for
the lists and folders they enumerate and keep current; `gramdrive-source` —
the `download` adapter implements the contract's ranged-read side
(`FetchRequest`, `ContentSink`, the `SourceError` taxonomy). External:
`serde_json` — JSON is tdjson's wire format and the config request type.
Dev-only: `gramdrive-state` — the snapshot, update, and folder-catalog
integration suites apply commits through the real typed repositories;
`gramdrive-testkit` — the ranged-read conformance run (SYNC-002) and its
verifying sink; product code never links either from here (the composing
caller owns that wiring).
Platform-specific code: forbidden — the keychain lives behind the
`SecretSource` seam, implemented in the native adapter. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-source-tdjson
```

Real-linkage smoke against the staged artifact (builds it first if needed
via `make tdlib`):

```sh
make tdjson-smoke
```
