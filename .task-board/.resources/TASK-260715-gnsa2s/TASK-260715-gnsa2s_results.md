# TASK-260715-gnsa2s — Apple shared durable state coordination: results

Date: 2026-07-18. Role: developer. Code-complete, all gates green, **ready
for review**.

## What landed

### 1. Rust state crate — corruption recovery + change probe (`crates/gramdrive-state`)

- **`src/recovery.rs`** — file-level corruption handling with two rules
  stated as contract: *detection is separate from destruction*
  (`probe_database` only reads — open without CREATE + `PRAGMA
  quick_check`; corrupt only on `SQLITE_CORRUPT`/`SQLITE_NOTADB`;
  `quarantine_if_corrupt` re-probes and declines healthy/missing files),
  and *one designated recovery owner* (enforced at the FFI boundary).
  Damaged files are preserved under `state/quarantine/<millis>-<pid>/`,
  never deleted. Move order shm → wal → **db last**: a crash
  mid-quarantine leaves a re-probeable main file, and a fresh database can
  never start life next to a stale `-wal` of its predecessor.
- **`StateStore::data_version()`** (`src/store.rs`) — SQLite's
  connection-relative change stamp: moves exactly when another connection
  (any process) committed since last read on this connection. The cheap
  "anything new?" probe change signaling pairs with. Documented as
  meaningless across handles/processes.
- **`StateError`**: new `QuarantineIo` variant + `is_database_corruption()`
  classifier (recurses through `MigrationFailed`; deliberately excludes
  row-level `CorruptRow`/`CursorCorrupt`, which can be version skew — a
  whole-file quarantine over one bad row would trade a bounded repair for
  total loss).

### 2. FFI shared-state surface (`crates/gramdrive-ffi/src/shared_state.rs`), contract 0.1.0 → 0.2.0 (additive)

- `shared_state_layout(data_root)` — one path rule for every process:
  `state/gramdrive.sqlite3`, `state/quarantine/`, `cache/`. Cache is
  deliberately under the data root, not an OS "Caches" location: the
  engine's quota accounting owns that space (system purge would invalidate
  it silently).
- `SharedStateStore.open(data_root, role)` — WAL-or-refuse, busy timeout,
  schema ensured; migrations run on open from **either** role (short,
  resumable, serialized by the write lock — SYNC-072). Roles differ in
  exactly one right: `Coordinator` (engine host) may recover;
  `Provider` (FP extension, UI) never destroys shared files.
- Snapshot reads: `item` / `children` (SYNC-003 anchored paging) /
  `child_by_name` — each one short WAL read snapshot; sync calls,
  documented for background queues. `data_version()`, `schema_version()`.
- `quarantine_corrupt_state(data_root, role)` — Provider refused with
  `InvalidArgument`; returns the quarantine dir or `None` (healthy/missing
  untouched).
- **No writes over FFI, deliberately**: durable state is written by the
  engine in-process (its host); a foreign write surface would invite the
  extension to mutate engine-owned state (DEC-006). The smoke's writer is
  a Rust process for exactly this reason (`examples/shared_state_seed.rs`),
  not a smoke-only contract API.
- Smoke consumers updated to assert contract 0.2.0
  (`.scripts/smoke/{swift/main.swift,kotlin/Main.kt}`).

### 3. Swift package `apple/GramDriveSupport` (first Apple-native product source)

- `AppGroup` — identifier `262RZ595FP.com.reluxworks.gramdrive` (DEC-019:
  the team-prefixed entitlement form v1 ships), container resolution, and
  the data-root rule `Library/Application Support/GramDrive`.
- `SharedState` — role-based open: `openInAppGroupContainer(role:)` for
  product processes, `open(dataRoot:role:)` for tests/tools.
- `ChangeSignal` — the cross-process doorbell: payload-free Darwin
  notification, App-Group-prefixed name (what sandboxed processes may
  post/observe); post-after-commit, observe → compare `dataVersion()` →
  re-read on change. Advisory, never authoritative. Finder signaling
  (`signalEnumerator`) explicitly left to the FP domain task.
- Consumes the **packaged** GramDriveCore (XCFramework + generated
  bindings) as a path dependency on `.temp/packaging/GramDriveCore`
  (`make package` stages it; `GRAMDRIVE_CORE_PACKAGE` overrides).
- `gramdrive-shared-state-smoke` executable: reader / watcher / doorbell
  modes for the harness.

### 4. Multi-process smoke (`make smoke-shared-state`, `.scripts/smoke/run_shared_state_smoke.py`)

Real separate processes over one substitute container:
Rust coordinator seeds → **two concurrent Swift provider processes read
byte-identical item metadata** through the packaged artifact (the
checklist's smoke proof) → a Swift watcher observes **both** the Darwin
doorbell (posted by a fourth process) **and** the `dataVersion` movement
across a Rust foreign commit, then re-reads the mutated facts → two
concurrent readers agree on the mutated state. Result:
`SHARED-STATE SMOKE PASSED` (logs `.temp/shared-state-smoke/`).

## AC evidence (multi-process stress and crash tests)

`crates/gramdrive-state/tests/multiprocess.rs` — real processes
(re-executed test binary; two connections in one process cannot prove
process death):

- **Stress**: 3 writer processes × 25 batches, each batch one message +
  the sealing cursor in one transaction (SYNC-022) plus a serialized
  read-modify-write counter bump; a live observer holds
  cursor-behind-state on every snapshot while they race. Terminal:
  gapless lanes, every cursor sealed at 25, counter exactly 75 (no lost
  update), `quick_check` healthy.
- **Crash**: writer SIGKILLed mid-stream, 3 rounds, resuming from its
  durable cursor. SIGKILL allows no rollback — WAL recovery on the next
  open must discard half-written work. After every kill: file healthy,
  observed commits durable, messages == exactly the cursor-sealed batches
  (nothing lost, nothing torn).
- No shared-memory assumptions anywhere: children coordinate with the
  parent via stdout/env only; all invariants read from the file.

Plus `tests/recovery.rs` (7 tests: deterministic corrupt fixtures,
healthy/missing declined, sidecar handling, fresh open after quarantine,
distinct quarantine dirs) and 10 new FFI unit tests (layout, role rights,
reads, paging, data_version on foreign commit, corruption as `Storage`,
recovery round-trip).

## Verification

| Check | Result |
|---|---|
| `make check` (suite all: toolchain, format, lint, test, architecture, supply-chain, traceability, scripts) | **8/8 ok** — provenance `.temp/acceptance/local-all` |
| `cargo test -p gramdrive-state` | all suites green incl. new multiprocess (3) + recovery (7) |
| `cargo test -p gramdrive-ffi` | 23 green |
| `swift test` (apple/GramDriveSupport, arm64, macOS 14 platform floor) | 11/11 green (Swift Testing) |
| `swift build` (package builds against packaged artifact) | ok |
| `make package` | PACKAGING PASSED (artifact carries contract 0.2.0) |
| `make smoke-bindings` | SWIFT + KOTLIN SMOKE PASSED (0.2.0 asserted) |
| `make smoke-shared-state` | SHARED-STATE SMOKE PASSED |

## Findings worth carrying (also in LOGBOOK.md 2026-07-18 1110)

1. **Darwin notification names are host-global** — parallel tests
   observing the product name hear each other; the package keeps an
   internal name-scoped seam and tests use unique names.
2. **Swift stdio block-buffers pipes** — harness-facing processes need
   `setbuf(stdout, nil)` (or explicit flushes) before line-synchronized
   protocols work.
3. `PRAGMA data_version` semantics (connection-relative; own commits do
   not move it) are exactly right for the doorbell pairing but wrong for
   any cross-process comparison — documented at every exposure.

## Files

- New: `crates/gramdrive-state/src/recovery.rs`,
  `crates/gramdrive-state/tests/{multiprocess,recovery}.rs`,
  `crates/gramdrive-ffi/src/shared_state.rs`,
  `crates/gramdrive-ffi/examples/shared_state_seed.rs`,
  `apple/GramDriveSupport/{Package.swift,README.md,Sources/GramDriveSupport/{AppGroup,SharedState,ChangeSignal}.swift,Sources/SharedStateSmoke/main.swift,Tests/GramDriveSupportTests/GramDriveSupportTests.swift}`,
  `.scripts/smoke/run_shared_state_smoke.py`
- Modified: `crates/gramdrive-state/src/{error,store,lib}.rs`,
  `crates/gramdrive-state/README.md`, `crates/gramdrive-ffi/{Cargo.toml,
  src/{lib,api}.rs, README.md}`, `.scripts/smoke/{swift/main.swift,
  kotlin/Main.kt}`, `Makefile`, `README.md`, `LOGBOOK.md`
- Nothing committed (workflow: commits happen after human review).

## Out of scope (owned by siblings)

- Agent lifecycle, launch policy, bounded IPC service — TASK-260715-1yx9ly.
- Companion shell UX (diagnostics/repair surfaces that would call
  `quarantine_corrupt_state` and `schema_version`) — TASK-260715-13pxnu.
- FP domain registration and Finder change signaling
  (`signalEnumerator`) — TASK-260715-3s44pc and the FP stories.
- Engine-side write flows and cache population — STORY-260715-2hs8cf.
