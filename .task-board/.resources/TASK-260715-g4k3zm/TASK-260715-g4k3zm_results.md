# TASK-260715-g4k3zm — Durable transfer state machine: implementation notes

## What was built

New module `gramdrive-engine::transfer` (the engine crate's first real code):

- `transfer/mod.rs` — `TransferMachine`, a *stateless* policy layer over the
  transfer journal persisted by `gramdrive-state`. The durable rows ARE the
  machine state; every operation is one short transaction that re-reads the
  row it acts on. Operations: `request` (validate + pin current content
  version + coalesce, SYNC-042/046), `claim` (with pin re-validation and a
  resume plan), `record_progress` (monotonic, extent-bounded, single staging
  handle), `checkpoint` (cancel > drift > continue), `acknowledge_cancel`,
  `invalidate`, `suspend`/`resume`, `request_cancel`, `fail` (SYNC-044
  classification + retry budget), `complete` (promotion gate).
- `transfer/ranges.rs` — canonical range-set arithmetic (normalize /
  subtract / covers / whole_object); the one place lists become sets.
- `transfer/retry.rs` — `RetryPolicy` (budget, deterministic exponential
  backoff, no jitter by design), `TransferFault` (source errors + local
  DiskFull/Integrity), exhaustive fault classification.
- `transfer/error.rs` — `EngineError` (NFR-030-style categories:
  NotHydratable, RangeBeyondExtent, IncompleteContent, UnknownExtent,
  ProgressRegression, StagingChanged, State passthrough).

## Key design decisions

1. **Claim token (`ClaimedTransfer`)**: progress/finish operations exist only
   on a claim, making "work on an unclaimed transfer" unrepresentable in the
   API; the durable row-level rules (repo `InvalidTransition`) still back the
   token against external movement.
2. **`complete` borrows the claim** instead of consuming it: a refused gate
   (IncompleteContent / UnknownExtent) changes nothing durable and leaves the
   claim usable. Consuming on refusal would strand a `running` row with no
   token to move it (suspend/fail need the token) — an API deadlock found
   while writing tests.
3. **Promotion gate**: coverage of the target set (requested, or `[0,size)`
   for whole-object) is checked in the same transaction as the repo's
   version-pin re-check and the `done` transition. Whole-object with unknown
   extent fails closed (`UnknownExtent`) until a metadata refresh records the
   size.
4. **Version races invalidate deterministically**: claim, checkpoint,
   completion, and source-reported `VersionConflict` all converge on one
   `discard` shape — wipe staged ranges + staging handle, terminal
   `failed`/`version_conflict` (or the precise category: NotFound /
   Restricted / Unavailable when the item departed harder), return a
   `StagingDisposal` for the host. No auto-re-enqueue: fresh demand
   re-requests at the current version (demand union is the fetch
   coordinator's job, TASK-260715-22fh09).
5. **Parking**: `AuthRequired` and `DiskFull` (and a source-observed local
   stop with no durable cancel flag) suspend with progress kept instead of
   burning retry budget or polling the queue. The "why parked" is reported to
   the caller but not persisted per-row: any resume just re-claims, and a
   still-failing precondition parks again — convergent, so hosts may resume
   coarsely ("after reauth, resume everything suspended").
6. **Retry budget** (NFR-033): persisted `retry_count` vs
   `RetryPolicy::retry_budget`; flood-wait minima outrank the policy schedule
   (SEC-031). Integrity failures wipe staged bytes and re-fetch from scratch
   under the same budget.
7. **Abandoned cancels**: a cancel flagged on a *queued* row is invisible to
   claims forever; the next `request` for the same (item, version) is what
   acknowledges it and starts fresh. (No repo API exists to list flagged
   rows; convergence-on-demand avoids adding one.)
8. **Terminal non-`done` rows claim no staging** (uniform invariant): every
   terminal path wipes ranges + `temp_ref` and hands the handle back as a
   disposal; startup reconciliation remains the backstop for dropped
   disposals. `done` rows keep ranges + handle as evidence for the promotion
   layer (TASK-260715-3s6cpe).

## Acceptance criteria → proof

- *Rejects invalid transitions*: typestate (claim-only operations) + repo
  passthrough tested (`progress_is_monotonic_...`, spent-claim test in
  `promotion_refuses_...`). Also monotonic progress, fixed staging handle,
  extent bounds.
- *Resumes after crash*: `an_interrupted_transfer_resumes_from_persisted_ranges_after_a_crash`
  — file-backed store, real reopen, `StateStore::reconcile` requeues the dead
  `running` claim with progress intact, resumed claim plans exactly the
  missing suffix, gate still refuses incomplete promotion before the suffix
  lands.
- *Never exposes incomplete content as valid*: promotion gate tests
  (incomplete refused with exact missing ranges; unknown extent fails
  closed; zero-byte object promotes with no staging; done rows only after
  full coverage + pin check).
- *Version race invalidates deterministically*: three tests covering drift at
  checkpoint, at claim, at completion, and source-reported conflict — all the
  same terminal residue.

## Verification run

- `cargo test -p gramdrive-engine`: 7 unit + 18 integration tests, all pass.
- `make check` (suite all): 8/8 ok — toolchain, format, lint (clippy
  `-D warnings`, workspace), test (full workspace), architecture,
  supply-chain, traceability, scripts. Provenance: `.temp/acceptance/local-all`.

## Files touched

- `crates/gramdrive-engine/src/transfer/{mod,ranges,retry,error}.rs` (new)
- `crates/gramdrive-engine/src/lib.rs` (module registration + docs)
- `crates/gramdrive-engine/tests/transfer_machine.rs` (new)
- `crates/gramdrive-engine/README.md` (module documented)

No `gramdrive-state` changes were needed — the existing repo API was
sufficient (see decisions 7 and 8 for the two places that was a close call).

Working tree left uncommitted per the no-auto-commit rule; ready for review.
