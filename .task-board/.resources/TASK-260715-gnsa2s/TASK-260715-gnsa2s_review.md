# TASK-260715-gnsa2s — Review verdict: ACCEPTED → done

Date: 2026-07-18. Role: reviewer (read-only; no code modified).

## Verdict

Accepted. Implementation matches the AC, fits the project architecture, and
every gate is green on independent reviewer re-runs (not trusted from the
developer's report).

## Independent verification (all re-run by the reviewer)

| Check | Result |
|---|---|
| `make check` (suite all) | 8/8 ok — toolchain, format, lint, test (`cargo test --workspace --all-features`), architecture, supply-chain, traceability, scripts; provenance `.temp/acceptance/local-all` |
| `cargo test -p gramdrive-state --test multiprocess --test recovery` | 3 + 7 green; verified the multiprocess suite spawns real re-exec'd children (assertions — counter exactly 75, sealed cursors — can only pass if children wrote; sub-second wall clock is legitimate WAL/NORMAL commit speed on arm64) |
| `swift test` (apple/GramDriveSupport) | 11/11 green, Swift Testing, arm64, macOS 14 platform floor |
| `make smoke-shared-state` | SHARED-STATE SMOKE PASSED on a fresh container: Rust coordinator seeds → two concurrent Swift provider processes read byte-identical metadata через packaged XCFramework → watcher observes Darwin doorbell AND moved dataVersion across a Rust foreign commit → two concurrent readers agree on mutated state |

## AC assessment

AC: "Multi-process stress and crash tests pass without shared-memory
assumptions or database corruption."

- Stress (`tests/multiprocess.rs`): 3 real writer processes × 25 SYNC-022
  batches + serialized read-modify-write counter; live observer holds
  cursor-behind-state on every snapshot; terminal state gapless, counter
  exactly 75 (no lost update), `quick_check` healthy. MET.
- Crash: writer SIGKILLed mid-stream × 3 rounds; after every kill the file
  probes healthy and messages == exactly the cursor-sealed batches. SIGKILL
  leaves no rollback path — WAL recovery proven to discard torn work. MET.
- No shared memory: children coordinate with the parent via stdout/env
  only; every invariant is read from the database file. MET.

Checklist items 1–3 (App Group layout per DEC-019, Swift package over the
packaged artifact with the two-process read-consistency smoke, gates green
on macOS 14 arm64) — all verified.

## Architecture fit

- Detection ≠ destruction: `quarantine_if_corrupt` re-probes internally and
  declines healthy/missing files — a misdiagnosed error cannot destroy
  state. Corruption trigger correctly narrowed to
  SQLITE_CORRUPT/SQLITE_NOTADB; row-level decode errors deliberately
  excluded (version skew ≠ file corruption).
- Move order shm → wal → db-last is the correct crash ordering: a crash
  mid-quarantine leaves a re-probeable main file and can never leave a
  stale `-wal` beside a fresh database.
- No writes over FFI honors DEC-006 (thin extension); the smoke writer is a
  Rust process — the product's real write shape, not a smoke-only API.
- `data_version` connection-relative semantics documented at every exposure
  (store, FFI, Swift doorbell pairing) — the exact contract POL/SYNC change
  signaling needs, and the exact trap if someone compares it across handles.
- Cache under the data root (not an OS Caches dir) protects the engine's
  quota accounting from system purge — consistent with the state-repositories
  design.
- Contract bump 0.1.0 → 0.2.0 additive; both bindings smoke consumers
  updated; docs (README, crate READMEs, LOGBOOK) complete and accurate.

## Notes (non-blocking)

1. `quarantine_corrupt_state(root, role)` — the role is caller-asserted;
   the FFI cannot see process identity, so coordinator-only recovery is a
   documented honor-system contract. Any alternative (role on the handle)
   is equally unenforceable; real enforcement would need entitlements.
   Acceptable for v1; every exposure states the rule.
2. `Package.swift` reads `GRAMDRIVE_CORE_PACKAGE` from the environment at
   resolution time — nonstandard but documented in the manifest header;
   fine for a dev-time path dependency, and a shipped app package will pin
   its own resolution.
3. `apple/GramDriveSupport/.build/` is present in the working tree but
   correctly gitignored (`.build/` pattern).

## Routing

`done`. Follow-on work is already owned by siblings (agent lifecycle
TASK-260715-1yx9ly, companion shell TASK-260715-13pxnu, FP domain
TASK-260715-3s44pc, engine write flows STORY-260715-2hs8cf).
