# TASK-260715-22fh09 — Ranged fetch coordinator: implementation notes

Status: implementation ready for review. All gates green (`make check`, 8/8,
provenance `.temp/acceptance/local-all`).

## What was built

New module `gramdrive-engine/src/fetch/` — the ranged fetch coordinator
(SYNC-041/043/044/045/046) driving any `DriveSource` through claims from the
durable `TransferMachine` (TASK-260715-g4k3zm):

- `fetch/mod.rs` — `FetchCoordinator`: reader registration (`open`/`close`),
  sink-less demand (`hydrate`), the attempt loop (`run_next`), reader
  streaming, re-request of uncovered remainders, fault settlement.
- `fetch/plan.rs` — pure chunk planning: widen remaining ranges to the chunk
  grid (SYNC-041), never re-fetch staged bytes, split on grid boundaries.
- `fetch/sink.rs` — `ChunkSink` + `SharedDelivery`: per-sub-fetch verified
  delivery (`FetchProgress`), staging writes, latched breakage (violation /
  staging error), in-band stop flag.
- `fetch/staging.rs` — host ports: `StagingHost`/`Staging` (offset-addressed
  scratch storage under a stable opaque handle) and `StagingError`
  (`Full` → DiskFull/park, `Failed` → Integrity/wipe+refetch).

Supporting changes: `transfer::ranges`, `transfer::item_standing` made
pub(crate); `gramdrive-testkit` added as a dev-dependency of the engine
(architecture-legal: dev-only); engine README/lib docs updated.

## Design decisions

1. **Runtime-agnostic, clock-free.** The engine has no async runtime and the
   architecture forbids one leaking in. The coordinator is plain
   `async fn` + a hand-rolled bounded-fanout multiplexer (`poll_fn` over the
   sub-fetch fleet, deterministic poll order). Time enters via a host `Clock`
   trait (SYNC-073). Tests drive everything on the testkit's single-threaded
   executor; the run future is `Send` (asserted in tests) so FFI can drive it
   on tokio.
2. **Sub-fetch shape.** Each planned chunk becomes one `source.fetch()` whose
   async block owns its `ChunkSink`; sinks share `Arc<Mutex<SharedDelivery>>`
   (staging + written spans + stop flag + breakage latch). No unsafe, no
   self-references, futures are individually droppable — dropping is the
   prompt per-chunk cancel (SYNC-005).
3. **Durable progress at chunk grain.** After each completion batch the
   written spans are folded into `record_progress`, then a
   `machine.checkpoint` runs (cancel outranks faults, drift outranks source
   errors). A checkpoint also runs before the first byte of every claim, so a
   cancel raised against a queued/claimed row costs zero network.
4. **Locator refresh in-attempt (SYNC-045).** `StaleReference` re-asks the
   source for the undelivered tail of the same chunk, same identity, bounded
   by `FetchConfig::stale_refresh_limit`; past the budget it classifies
   through `machine.fail` like every other fault. All other taxonomy
   (flood-wait minimums, park classes, retry budget) stays in the machine —
   the coordinator adds no second retry policy.
5. **Readers stream from staging.** Reader delivery is contiguous per reader,
   read back from staging as coverage advances — this makes out-of-order
   parallel sub-fetches, resume-from-staged, and coalesced late-attaching
   readers all one code path. A reader is pinned to the version it opened
   (never topped up across versions); uncovered remainders are re-requested
   when the live transfer finishes (`ReaderEnd::Reattached`), per the
   machine's documented contract. Readers on requeued/parked transfers stay
   subscribed.
6. **Unknown extent fails closed.** A whole-object claim with no recorded
   size suspends (`AttemptEnd::ExtentUnknown`) instead of probing blind;
   a metadata refresh + `resume` continues it. Mirrors the promotion gate's
   fail-closed stance.
7. **Cache read path is out of scope** (TASK-260715-11abx8/3s6cpe): a reader
   opened after promotion starts a fresh transfer. Documented in module docs.

## Acceptance criteria → evidence (tests/fetch_coordinator.rs, 17 tests)

- **Range bytes are correct**: `single_reader_streams_exact_bytes_and_aligns_chunks`
  (exact [5,23) via contract-verifying sink; aligned [0,16)+[16,32) on the
  wire), `concurrent_readers_coalesce_onto_one_transfer`, crash-resume test
  asserts full 64-byte reassembly.
- **Cancellation is prompt**: `queued_cancel_prevents_any_network_and_new_demand_displaces`
  (zero fetches), `durable_cancel_mid_transfer_stops_promptly_and_disposes_staging`
  (two SQLite connections; cancel observed at the next checkpoint, remaining
  chunks never fetched, staging disposal returned, row `cancelled`, progress
  wiped), `dropped_run_future_leaves_resumable_state` (drop = local cancel;
  reconcile → resume without re-fetching staged bytes),
  `reader_stop_unsubscribes_without_stopping_the_transfer` (in-band stop).
- **Stale version cannot publish**: `version_race_invalidates_and_never_publishes_stale_bytes`
  (mid-fetch race after 10 bytes → `Invalidated`, row terminal
  `failed/version_conflict`, staged wiped, disposal returned, readers failed;
  re-open pins v2 and fetches clean bytes). Promotion goes only through
  `machine.complete`'s atomic gate.
- **Duplicate compatible network work is bounded**: coalescing test (shared
  bytes cross the wire once), retry test (staged chunk not re-fetched),
  crash-resume test ([0,16) fetched exactly once across the crash),
  `parallelism_is_bounded_within_one_item` (fanout=2 → exactly 2 in flight).

Also covered: priority ordering across items, flood-wait honored over policy
backoff (`next_retry_at_ms == now+30s`), stale-reference refresh budget →
retry queue → terminal, disk-full park + resume, unknown extent, zero-size
object promotion without staging, contract-violating source → terminal
`Internal` with no violating byte reaching the reader.

Unit tests: `fetch/plan.rs` (9, incl. overflow at u64::MAX), `fetch/sink.rs`
(4), `fetch/staging.rs` (1), `fetch/mod.rs` (2).

## Commands run

- `cargo build -p gramdrive-engine` — ok
- `cargo clippy -p gramdrive-engine --all-targets` — clean
- `cargo test -p gramdrive-engine` — 18 transfer_machine + 17 fetch_coordinator + unit tests, all green
- `make fmt && make check` — 8/8 gates green (toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts)

## Notes for the next tasks

- TASK-260715-3s6cpe (integrity/promotion) layers over
  `AttemptEnd::Promoted { staging }` — the handle holds coverage-complete
  bytes; hashing/materialization go there, plus a cache read path so
  post-promotion opens stop re-fetching.
- TASK-260715-11abx8 (quota/eviction) can meter `StagingHost` writes;
  `StagingError::Full` already parks with progress kept.
- Jitter deliberately absent (determinism); hosts add it when scheduling
  `run_next`, per `RetryPolicy::backoff_ms` docs.
