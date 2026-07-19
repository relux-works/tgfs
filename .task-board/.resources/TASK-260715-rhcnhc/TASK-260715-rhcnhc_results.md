# TASK-260715-rhcnhc — Enumerators and change anchors: implementation notes

## What landed

Paged child + working-set enumeration and durable change enumeration/signaling
for the macOS File Provider extension, built on a new core-side item change
journal (the durable half the task's "change journal" DoD required but the
core did not yet have).

### Rust core (prerequisite, additive)

- **Schema v2 — the first real migration** (`crates/gramdrive-state/src/schema/v2.sql`,
  `SCHEMA_VERSION` 1 → 2, registered in `src/migrate.rs`, exercised against the
  v1 fixture by `the_shipped_v2_migration_creates_the_item_change_journal`):
  - `item_changes`: the coalesced provider-visible change journal — at most one
    row per item, carrying the `AUTOINCREMENT` sequence of its *latest* change.
    Bounded by item count; issuance never rewinds (high-water mark read from
    `sqlite_sequence`, so cascade deletes cannot lower it). FK ON DELETE CASCADE
    keeps journal rows exactly as long as their item rows.
  - `item_change_journal.instance_id` (seeded via `randomblob(16)`): names the
    database life, so anchors from a quarantined-and-reseeded file are
    recognizably foreign instead of silently pointing at unrelated sequences.
  - No backfill, deliberately: pre-journal items have no changes to report; a
    provider without an anchor full-enumerates and adopts the current sequence.
- **Write-path journaling** (`repo/items.rs` — provably the only `items`
  writers): `upsert_item`, `update_item_content`, `tombstone_item` journal via
  `repo/item_changes.rs::journal_item_change` **only on actual provider-visible
  change**. The no-op discipline is required, not an optimization: the engine
  re-baselines identical rows after restart (SYNC-021 replay), and a blind
  journal would replay whole trees at the provider boundary on every restart.
  `upsert_item` compares the stored row over exactly the ON CONFLICT column set;
  identical content republish and tombstone-of-tombstone are equally quiet.
- **Reads** (`repo/item_changes.rs`): `change_journal_state()` (instance id +
  high-water mark) and `item_changes_since(account, after, limit)` (sequence-
  ordered pages joined live against `items`; a tombstone pages as a deletion).
- **FFI** (`crates/gramdrive-ffi/src/shared_state.rs`): `ItemChange`,
  `ChangeJournalState`, `SharedStateStore.change_journal_state()` /
  `item_changes_since(account_id, after_sequence, limit)`. Contract 0.3.0 →
  **0.4.0** (additive). `GramDriveCore` repacked (`make package` PASSED,
  manifest at `.temp/packaging/manifest.json`).

### Swift (`apple/GramDriveSupport/Sources/GramDriveFileProvider/`)

- **`GramDriveEnumerator`** — the one enumerator type:
  - *Listing (SYNC-003, NFR-021):* keyset pages over `children` in stable core
    id order; the continuation page records the last delivered id, so no
    duplicates and no misses across pages, memory bounded by page size
    (default 256, capped further by the observer's suggestion). Directory
    liveness checked per callback; the account root of a not-yet-synced
    account lists empty rather than absent; missing/tombstoned containers
    answer `noSuchItem`.
  - *Changes:* pages the journal from the anchor's sequence; updates as full
    current items, POL-3 tombstones as deletions; finishes at the last
    delivered sequence with `moreComing` when the batch is full.
  - *Working set:* the domain-wide change feed; item enumeration answers an
    empty listing (macOS enumerates only working-set *changes*).
  - *Deadlines/cancellation:* every callback answers synchronously from short
    snapshot reads; `invalidate` has nothing in flight to cancel.
- **`EnumerationPageCursor`** — versioned page codec binding the container:
  both initial-page sentinels start from the beginning; anything foreign
  answers `NSFileProviderError(.pageExpired)` (the platform's explicit
  restart) rather than a guessed position that could duplicate or skip.
- **`EnumerationSyncAnchor`** — versioned anchor codec binding
  {account, namespace epoch (DOM-021), journal instance, sequence}; any
  mismatch or never-issued sequence answers `.syncAnchorExpired` (the
  explicit full-resync recovery). Minting is stateless: an anchor minted
  before a listing means concurrent commits are *behind* it and replayed by
  the next change enumeration — over-delivery is idempotent, loss would not be.
- **`ChangeSignalRelay`** — doorbell → Finder bridge: observes the Darwin
  doorbell, probes `dataVersion()`, and calls
  `signalEnumerator(for: .workingSet)` (through the `WorkingSetSignaling`
  seam over `NSFileProviderManager`) only when the probe moved. Probe-on-start
  covers rings missed while not running. Built and tested; hosting it in the
  domain-registering process is the engine-host/content stories' wiring.
- **`FileProviderExtension.enumerator(for:request:)`** — wired: working set,
  root, and live directories enumerate; unknown/tombstoned/unparseable
  containers answer `noSuchItem`; file containers and the trash of this
  read-only domain answer `featureUnsupported`.

## Acceptance criteria → evidence

- **No duplicate/missing fixture items**: keyset paging pinned by
  `EnumeratorListingTests.pagesCompose` and the scripted concurrent-update
  suite (`EnumeratorConcurrencyTests`: insert/rename/tombstone *between*
  pages — never a duplicate, never a resurrect; what listing cannot see, the
  change feed replays from the pre-listing anchor). Core-side page/journal
  semantics pinned in Rust (`repo_item_changes.rs`; FFI journal walk test).
- **Invalid cursors recover explicitly**: foreign/undecodable pages →
  `.pageExpired`; foreign, epoch-bumped, other-life, and overtaking anchors →
  `.syncAnchorExpired` (all four cases parameterized in
  `EnumeratorChangeTests.expiredAnchors`). Never a silent wrong diff.
- **Callback deadlines are met**: all callbacks complete synchronously before
  returning (structural; pinned by `synchronousCompletion` and by every test
  asserting immediately after the call).

## Gates (all run after the changes)

- `make check` — 8/8 PASSED (`.temp/acceptance/local-all`; log
  `.temp/TASK-260715-rhcnhc/check-01.log`)
- `swift test` (apple/GramDriveSupport) — **194/194 in 40 suites** PASSED
- `make package` — PASSED, contract 0.4.0 (log
  `.temp/TASK-260715-rhcnhc/package-01.log`)
- `make smoke-shared-state` — PASSED on the repacked artifact (log
  `.temp/TASK-260715-rhcnhc/smoke-shared-state-01.log`)
- `cargo fmt` + `cargo clippy --workspace --all-targets` — clean

## Testing-strategy note (DEC-006)

Swift tests cannot seed a real store (no writes over FFI, by design), so the
enumerator suites run over `ScriptedStore` — an in-memory
`SharedStateStoreProtocol` restating exactly the core semantics the Rust
suites pin (keyset order, coalesced journal, no-op quiet, monotonic
issuance) — with per-`children`-call mutation scripting as the
concurrent-writer seam. The real-store, cross-process proof remains
`make smoke-shared-state`.

## Known follow-ups (out of scope here)

- Host `ChangeSignalRelay` in the companion/agent next to domain
  registration (engine-host story); nothing durable is lost by later wiring.
- Old-epoch rows after a namespace bump are served/withheld identically by
  `children` and the journal (both unfiltered by epoch); epoch sweep belongs
  to reconciliation, and anchors already expire on the bump.
