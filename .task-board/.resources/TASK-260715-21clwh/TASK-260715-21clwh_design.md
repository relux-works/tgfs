# TASK-260715-21clwh — startup reconciliation design

## Where it lives

`crates/gramdrive-state/src/reconcile.rs`. `crates/README.md` assigns
"reconciliation" to `gramdrive-state`; `repair.rs` already models the marker
handoff and names this task as the side that clears them.

## Boundary problem: the cache directory

The state crate is platform-neutral by architecture rule (checks 6–9 of
`.scripts/check_crate_architecture.py`) and never chooses paths. So it cannot
walk a cache directory itself. Solution: a `LocalStorage` port the host
implements. The schema already carries the two opaque handles the port is
keyed by:

- `cache_entries.materialization_ref` — the materialized object
- `transfers.temp_ref` — the transfer staging area

Reconciliation compares the DB's ref sets against the host's inventory. No
paths, no `std::fs`, no target_os.

## Boundary problem: who owns a `running` transfer

> **Superseded during implementation.** The `InFlightPolicy` design below was
> built, passed the gate, and was then found wrong — see "Correction" at the
> end of this section. The shipped design has no policy flag. Kept here
> because the reasoning that killed it is the reason the final design looks
> the way it does.

PLAT-MAC-003 / architecture.md:137 — the app and the File Provider extension
are separate processes over one file. A `running` transfer row is therefore
ambiguous to the state crate: it is either a crashed previous run of *this*
process, or a live peer's in-flight work. The crate has no liveness primitive
(no pid, no platform API) and must not guess.

The topology is a host fact, so the host declares it, exactly as it already
declares the database location:

- `InFlightPolicy::Reclaim` — "I am the only process that claims transfers"
  (the containing app: TDLib lives there, architecture.md:84, so the engine
  does too). `running` rows are dead work from a previous run of this process.
- `InFlightPolicy::Leave` — "someone else may be claiming right now" (the
  extension). `running` rows are untouched and unreported.

Rejected alternative: lease/owner columns on `transfers` (migration v2). It is
the topology-independent answer, but lease *renewal* is engine behavior and
`gramdrive-engine` is empty — the protocol would have no participant except
its own tests. Revisit when the engine lands and if a second claimer ever
appears. (This rejection still stands.)

### Correction: the flag was unsound, so the pass takes a precondition instead

`Leave` does not actually make the pass safe, it only makes the *transfer*
check safe. Under `Leave` a live engine is still racing every other check:

- orphan detection deletes an object the engine wrote a moment ago and whose
  row it has not committed yet;
- leaked-staging detection deletes a staging area the engine allocated and
  whose `temp_ref` it has not recorded yet.

There is no per-finding patch, because *every* check here reads a
database/disk disagreement as damage and a live engine is a permanent,
legitimate source of exactly those disagreements. Nor is there an engine
write-ordering contract to lean on — `gramdrive-engine` is 32 lines.

So the pass takes the `fsck` precondition: **nothing else may be touching what
it repairs**. One rule, no flag, no per-finding subtlety, no assumption about
engine internals — and it is what "startup reconciliation" already meant. It
is a caller contract this crate cannot check, the same shape as "the host
chooses where the file lives". The app runs the pass at startup before
claiming anything; the extension never runs it (it claims and materializes
nothing, so it has nothing to reconcile and no standing to repair the app's
state); TASK-260715-1nuhxj quiesces the engine first.

That precondition is also what makes `running` legible: with no engine live,
no claim can be live, so every `running` row is a dead one — a fact, not an
inference. `ReconcileOptions` collapses to a plain `now_ms: i64`, matching
every other clock-free API in the crate.

## What the pass does

Read the whole survey under one snapshot, then apply repairs in short write
transactions. Every finding is derived from durable evidence only.

| Finding | Evidence | Repair |
|---|---|---|
| `InterruptedTransfer` | `state = 'running'` (a dead claim, per the precondition) | → `queued`, **keeping** `completed_ranges`/`temp_ref` (roll forward, not restart); `retry_count` untouched — a crash is not a failed attempt |
| `LeakedStaging` | staging object no *live* transfer claims via `temp_ref` | delete the object; clear `temp_ref` off the terminal row |
| `MissingCacheObject` | `materialization_ref` absent from the inventory | drop the `cache_entries` row (the bytes are gone; the row is the lie). **`pins` survives** — POL-2 intent is independent of materialization, which is the AC's "no loss of valid pinned content". A generated doc also goes back on the dirty worklist |
| `UnlocatableCacheEntry` | entry with `materialization_ref IS NULL` | report only — nothing to check it against |
| `OrphanCacheObject` | inventory object no `cache_entries` row claims | delete the object |
| `ProjectionRebuildPending` | `rebuild_projection` marker | report only; the marker stays raised. Rebuilding `items` needs the projection builder, which is engine-side and does not exist yet |
| `MigrationInterrupted` | `migration_interrupted` marker | report only; `StateStore::open` is what resumes it |

A `temp_ref` a live row still claims is protected: the transfer this pass just
requeued resumes from exactly those bytes.

### Dirty shutdown

`dirty_shutdown` = the previous run left in-flight work behind. Evidence: a
`running` transfer, leaked staging, or a still-raised `migration_interrupted`
marker. Each is a leftover by definition, since no engine is live.

### AC: "without Telegram writes"

Structural. The crate cannot depend on `gramdrive-source` (crates/README.md
allow list) and `reconcile` takes only `&dyn LocalStorage` and a timestamp.
There is no source handle to misuse.

## API

```rust
StateStore::plan_reconcile(&mut self, &dyn LocalStorage) -> Result<ReconcilePlan, StateError>
StateStore::reconcile(&mut self, &dyn LocalStorage, now_ms: i64) -> Result<ReconcileReport, StateError>
```

`plan_reconcile` is the dry-run TASK-260715-1nuhxj wraps as its user-triggered
entrypoint; `reconcile` is the automatic startup pass. A storage removal that
fails becomes an `Unresolved` finding, never an aborted pass — convergence is
the point.

## Tests

`tests/reconcile.rs`, fake `LocalStorage`. Crash tests use a re-exec'd child
that `std::process::abort()`s with work in flight — a real process death, no
platform dependency, no signal plumbing. Then: reopen, reconcile, assert
convergence, and assert a second reconcile finds nothing (idempotence).
