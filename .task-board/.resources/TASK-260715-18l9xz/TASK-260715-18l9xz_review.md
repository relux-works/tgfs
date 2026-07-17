# TASK-260715-18l9xz — Review: schema migration framework

**Verdict: ACCEPTED → `done`.** All four AC met with load-bearing evidence,
`make check` 8/8 re-run independently, solution fits the crate's
architecture. One latent gap found and recorded for follow-up (below); it is
not rework against this task's AC and does not affect the shipped build.

Reviewer: [reviewer] reviewer (claude), 2026-07-17.

## Acceptance criteria

| AC | Verdict | Evidence checked |
|---|---|---|
| Fixture from prior schema | met | `fixtures/v1_seed.sql` (143 lines) is real and load-bearing — every runner test runs against it, built from the frozen `v1.sql` with FKs on and `PRAGMA foreign_key_check` asserted clean (`the_v1_fixture_is_a_real_v1_database`). `every_migration_ships_a_fixture_of_the_schema_it_migrates_from` enforces it for future migrations |
| Interruption test | met | `an_interrupted_migration_resumes_from_its_checkpoint` — file-backed DB, connection dropped, file reopened from scratch; asserts version never moved and exactly the committed chunks survived. `the_preamble_survives_a_rollback_and_is_never_applied_twice` covers the DDL preamble |
| Idempotent resume | met | `resuming_produces_exactly_what_an_uninterrupted_run_produces` — interrupted **twice**, then compared row-for-row against a clean run. This is the right formulation: the property is that interruption is unobservable in the result |
| Clear incompatible-version error | met | `a_file_from_a_newer_build_is_refused_before_anything_is_written_to_it` — typed `UnsupportedSchemaVersion{found, supported}`, and asserts no journal tables appear, proving the check precedes the first write. Verified in `schema.rs:47-69` that the ordering is real, not incidental |

## Verification performed

- `make check` → 8/8 green, re-run by the reviewer (not taken on trust).
- `cargo test -p gramdrive-state --lib -- --test-threads=1` → 14/14. Checked
  specifically because the test migrations coordinate through `thread_local!`
  counters (`FAIL_AFTER_CHUNKS`, `CHUNKS_RUN`) and `disarm_failure()` does
  not reset `CHUNKS_RUN`. libtest spawns a thread per test regardless of
  `--test-threads`, so the isolation holds. **Not a defect.**
- `v1.sql` scanned for transaction-hostile statements before
  `apply_baseline` wraps it in one — the only `BEGIN` is a trigger body
  (`v1.sql:162`), no `PRAGMA`. Baseline-in-one-transaction is sound.
- Spec references spot-checked: SYNC-071/072
  (`.spec/sync-and-filesystem-semantics.md:82-83`) and NFR-041
  (`.spec/quality-and-release.md:44`) say what the code claims they say.

## What is good, specifically

The central invariant — `user_version` advances only in the transaction that
earns it — is stated once and actually held everywhere: `finish()` is only
ever called inside a caller-owned transaction, and `apply_resumable` commits
each chunk's data, its checkpoint, and its repair marker as one unit. The
`prepare`-inside-the-first-chunk-transaction trick is the non-obvious part
and it is correct: gated on `checkpoint.is_none()`, so it either commits with
the first chunk or rolls back with it, and can never be applied twice.

The const assertion tying `MIGRATIONS` to `SCHEMA_VERSION` is the right
mechanism — it makes `MigrationRequired` unreachable in a correct build while
keeping it a typed error rather than an assumption, and the reasoning for
keeping it is written down.

Mutation results are credible and the implementer reported an honest negative
(logbook 1656: one mutation produced an infinite loop, not a test failure,
and the limit is documented on `ChunkFn` rather than papered over). The
journal-outside-the-numbered-schema decision resolves a genuine circularity
and is argued, not asserted.

`MIGRATIONS` being empty is correct, not a shortfall: `SCHEMA_VERSION` is the
baseline, so there is no version to migrate to. The per-migration AC is
enforced structurally for the first real migration instead.

## Finding — cross-process TOCTOU in the migration runner (latent, non-blocking)

`migrate::run` reads the version outside any transaction, and `apply` never
re-checks it inside the transaction it opens:

```rust
let mut current = current_version(conn)?;   // migrate.rs:200 — no transaction
while current < target {
    ...
    apply(conn, migration)?;                // opens its own tx, stamps the version
}
```

`rusqlite`'s `conn.transaction()` is `DEFERRED`, so nothing serializes two
processes that both read v1 and both decide to apply v2. The crate's premise
is exactly that two processes share this file (`lib.rs:6-9` — app + File
Provider extension), and migrations run inside `StateStore::open`, so a first
launch after an upgrade that ships a v2 is precisely when both race.

Reproduced at the SQLite level (`.temp/TASK-260715-18l9xz/concurrent_migration_probe.py`,
mirroring `run`/`apply` statement-for-statement):

```
A sees v1, B sees v1 -> both decide to apply migration v2
  A: applied v2, committed
  B: FAILED -> OperationalError: duplicate column name: render_hint

file ends at v2, schema_history=[(1,), (2,)], columns=[chat_id, message_id, render_hint]
```

**Severity is bounded, and deliberately so.** The file ends up correct — the
`schema_history` PRIMARY KEY on `version` means a double-apply can never
silently succeed even if the migration SQL were idempotent, and the
version-stamp invariant holds throughout. There is no corruption. The impact
is that the losing process's `StateStore::open()` returns
`Err(MigrationFailed)` against a perfectly healthy database, with no retry —
on Apple that is the File Provider extension failing to start until it is
relaunched.

**Why this is not rework against this task.** Concurrency is not in the AC,
nothing in the code, README, or results claims cross-process migration
safety, and it is unreachable in the shipped build because `MIGRATIONS` is
empty. The remedy is also a design choice, not a typo: `TransactionBehavior::
Immediate` plus a version re-read inside the transaction (treating
"already at this version" as success) is the standard shape, but "fail fast
vs. wait vs. recheck-and-skip" deserves its own decision rather than a
reviewer-mandated patch at review time.

**Why it should not be left implicit.** The framework is the deliverable and
this is what a v2 gets written against; the docs enumerate every other
failure path exhaustively, so the omission reads as coverage. Recommend a
follow-up task under STORY-260715-16ik2x, sequenced before the first real
migration ships. Recorded in logbook 1659.

## Nits (non-blocking, no action required)

- `lib.rs:39` exports `Migration`, `MigrationStep`, `ChunkFn`, `ChunkOutcome`
  publicly, but `MIGRATIONS` and `run` are `pub(crate)` — an external crate
  can construct a `Migration` and do nothing with it. Harmless and arguably
  useful, since the public `StateError` docs reference these types.
- `store.rs:72` sets `journal_mode = WAL` before `ensure_schema` runs the
  future-version check. A no-op on a future file (already WAL), so the
  "refused before anything is written" claim holds — but the assertion in
  `a_file_from_a_newer_build_is_refused_before_anything_is_written_to_it`
  covers journal *tables*, which is narrower than the test name promises.
