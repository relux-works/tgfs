# TASK-260715-22fh09 — Review verdict: ACCEPTED → done

Reviewer: reviewer (claude), 2026-07-17. Read-only review of the working
tree (nothing committed, per workflow rules); all verification commands
rerun by the reviewer, not taken from the implementer's notes.

## Verification rerun

- `make check` — 8/8 gates green (toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts); provenance
  `.temp/acceptance/local-all`.
- `cargo test -p gramdrive-engine` — 23 unit + 17 `fetch_coordinator`
  integration + 18 `transfer_machine` integration, all green.

## AC → evidence (code read + tests rerun)

1. **Range bytes are correct.**
   `single_reader_streams_exact_bytes_and_aligns_chunks`: demand `[5,23)`
   streams exactly `content[5..23]` through the testkit's
   contract-verifying `RecordingSink` (no violation, complete), while the
   wire sees aligned `[0,16)+[16,32)` (SYNC-041) and the row records the
   staged superset `[0,32)`. `dropped_run_future_leaves_resumable_state`
   reassembles all 64 bytes across a crash. Delivery integrity is enforced
   at the sink before staging (`fetch/sink.rs`: verify → write → account;
   a violating chunk never reaches staging or a reader —
   `contract_violating_source_fails_terminal`).
2. **Cancellation is prompt.**
   Queued: `queued_cancel_prevents_any_network_and_new_demand_displaces` —
   zero fetches ever issued. Running:
   `durable_cancel_mid_transfer_stops_promptly_and_disposes_staging` —
   cancel injected from a second SQLite connection mid-run is observed at
   the next work boundary; remaining chunks never fetched; row terminal
   `cancelled`, progress wiped, staging disposal returned. Local:
   dropping the `run_next` future drops all in-flight source fetches
   (futures own their sinks, no detached work) and reconcile → resume
   continues from staged bytes. In-band `SinkControl::Stop` path also
   covered (`reader_stop_unsubscribes_without_stopping_the_transfer`).
3. **Stale version cannot publish.**
   `version_race_invalidates_and_never_publishes_stale_bytes`: race after
   10 bytes → `AttemptEnd::Invalidated`, row terminal
   `failed/version_conflict`, `completed_ranges` wiped, `temp_ref` cleared,
   disposal returned, attached readers failed; re-open pins v2 and fetches
   clean bytes. Structurally, the coordinator never publishes — promotion
   goes only through `machine.complete`'s atomic coverage + version-pin
   gate; readers are version-pinned at open and never topped up across
   versions (`fetch/mod.rs` reattach path re-checks `item_standing`).
4. **Duplicate compatible network work is bounded.**
   `concurrent_readers_coalesce_onto_one_transfer` (overlapping demand,
   shared bytes cross the wire once);
   `retry_requeues_with_backoff_and_honors_flood_wait` and the crash test
   (staged chunks never re-fetched); `plan.rs` subtracts staged from the
   widened grid before splitting; `parallelism_is_bounded_within_one_item`
   (fanout=2 → exactly 2 in flight, observed mid-poll).

Description/scope items also verified: reader coalescing (SYNC-046),
backend chunk alignment (SYNC-041), locator refresh in-attempt with
identity unchanged and a bounded budget that then defers to the machine's
taxonomy (SYNC-045/044, incl. flood-wait minimum honored over policy
backoff), streaming to sinks from staging, concurrent opens, version
changes at claim/checkpoint/complete, disk-full park/resume, unknown
extent fail-closed, zero-size promotion without staging.

## Architecture fit

- Coordinator keeps no durable state — reconstructible from the journal
  plus open readers; durable policy stays single-homed in
  `TransferMachine` (no second retry policy; the only coordinator-private
  reaction is the SYNC-045 refresh, as specified).
- Runtime-agnostic and clock-free: host `Clock` + `StagingHost` ports,
  hand-rolled deterministic `poll_fn` fleet, run future `Send`
  (test-asserted). No new runtime/platform dependencies; architecture gate
  green.
- `gramdrive-testkit` added as dev-dependency only (legal per
  crates/README.md); `transfer::ranges`/`item_standing` widened to
  `pub(crate)` — proportionate, no public-surface leak.
- Layering vs TASK-260715-3s6cpe (integrity/promotion consumes
  `AttemptEnd::Promoted { staging }`) and TASK-260715-11abx8
  (quota/eviction meters `StagingHost`) is clean; cache read path
  explicitly out of scope and documented in module docs.

## Non-blocking observations (no rework)

- `FetchConfig::stale_refresh_limit` is a per-chunk budget (each
  sub-fetch carries its own `refreshes` counter), which the field docs do
  state; worth remembering when tuning, since a many-chunk attempt can
  refresh once per chunk.
- With fanout > 1, a latched sink breakage is attributed to whichever
  completion settles first, not necessarily the chunk that latched it —
  the fault classification is identical either way, so behavior is
  unaffected.
- `FetchCoordinator::close` scans all subscriptions to find the reader —
  O(readers), fine at realistic scale.

## Verdict

Accepted. Status → done.
