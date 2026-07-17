# TASK-260715-18l9xz — Schema migration framework

Status: ready for review. All 8 gates green (`make check`, suite `all`).

Requirements: SYNC-072 (resumable, crash-safe migrations), NFR-013
(transactional-or-resumable upgrades, documented rollback expectations),
NFR-041 (explicit versioning/migration tests), SYNC-071 / NFR-034 (repair
handoff).

## What was built

| File | What |
|---|---|
| `src/migrate.rs` | The runner: `Migration`, `MigrationStep`, `ChunkFn`, `ChunkOutcome`, the registry and its const assertion, + unit tests |
| `src/repair.rs` | Repair markers: `RepairKind`, `RepairMarker`, raise/clear/list |
| `src/schema/journal.sql` | `migration_progress`, `repair_markers` — meta-schema, outside the numbered sequence |
| `fixtures/v1_seed.sql` | The v1 fixture database a v2 migration will be tested against |
| `tests/migrations.rs` | The public surface |
| `src/schema.rs`, `src/error.rs`, `src/store.rs`, `src/lib.rs`, `README.md` | Wiring, typed errors, public API, docs |

## The design in one rule

`PRAGMA user_version` advances **only in the same transaction as the work
that earns it**. The version is therefore never a claim the data cannot
back, and that single rule is the whole crash-safety argument.

- `MigrationStep::Sql` — one transaction. Crash → rollback → next open
  starts over.
- `MigrationStep::Resumable { prepare, chunk }` — for data too large for one
  transaction (a backfill across 110k messages holds a write lock for the
  duration and loses everything to one crash at the end). Each chunk commits
  its data changes **together with** the checkpoint it resumes from, so the
  two can never disagree. `prepare` (the `ALTER TABLE` the chunks need) runs
  in the same transaction as the first chunk's commit: applied once, rolled
  back with it if that commit never happens.

Forward-only (NFR-013). No downgrade: an older build meeting a newer file
refuses it (`UnsupportedSchemaVersion`) rather than guessing what a newer
schema's data means in an older shape. Documented in the crate README.

## Acceptance criteria

| AC | Where | Note |
|---|---|---|
| Fixture from prior schema | `fixtures/v1_seed.sql`; `every_migration_ships_a_fixture_of_the_schema_it_migrates_from` | The v1 fixture exists and is **load-bearing today**: every runner test runs against it |
| Interruption test | `an_interrupted_migration_resumes_from_its_checkpoint`, `the_preamble_survives_a_rollback_and_is_never_applied_twice` | File-backed DB, connection dropped, file reopened |
| Idempotent resume | `resuming_produces_exactly_what_an_uninterrupted_run_produces` | Interrupted **twice**, then compared against a clean run |
| Clear incompatible-version error | `a_file_from_a_newer_build_is_refused_before_anything_is_written_to_it` | Asserts no journal tables appear after refusal — the refusal precedes the first write |

`MIGRATIONS` is empty, correctly: `SCHEMA_VERSION` is still the baseline, so
there is no version to migrate *to*. The per-migration AC is therefore
vacuous today and enforced structurally instead — the fixture test fails the
moment a migration is added without one, and a const assertion fails the
build if `MIGRATIONS` and `SCHEMA_VERSION` ever disagree. The runner itself
is exercised against the real v1 fixture with a test migration of the shape
this framework exists for (`ALTER TABLE` + chunked backfill).

## Verification

`make check` → 8/8 (toolchain, format, lint, test, architecture,
supply-chain, traceability, scripts). Provenance: `.temp/acceptance/local-all`.
20 new tests (14 unit + 6 integration); pre-existing suites unaffected.

**Mutation-tested** — a green suite proves nothing until it can fail:

| Mutation | Caught by |
|---|---|
| `user_version` stamped before the work finishes | 5 tests |
| Preamble re-applied on every chunk | 5 tests |
| Interruption marker never cleared | 2 tests |

## Findings (logbook 1656–1658)

1. **POL-3 append-only trigger blocks in-place payload migration.** The
   trigger permits exactly one UPDATE shape (payload → NULL, the Mirror
   purge), so a `payload_schema` 1→2 migration cannot rewrite payloads in
   place. Options for whoever bumps it: lazy read-time interpretation; drop
   and recreate the trigger (leaves the log unprotected across chunk
   transactions); or append new-schema revision events, which is what POL-3's
   model implies. **Not decided here** — no payload v2 exists. This is why
   the test migration backfills `messages` rather than `message_events`.
2. **Journal outside the numbered schema** — otherwise circular: a
   pre-runner file has no journal, and the runner needs one to run the
   migration that would create it. Bootstrap runs only after the
   future-version check.
3. **Stall guard limit** — catches a chunk returning the checkpoint it was
   given; cannot catch one returning a fresh useless checkpoint forever
   (indistinguishable from a long migration). Any runner-imposed bound would
   be a guess. Documented on `ChunkFn`.

## Scope note

The task's scope line reads "SQLite and serialized durable formats". The
serialized side is covered by the framework's ability to migrate serialized
payloads (the resumable step exists for exactly that shape), not by a new
payload-compatibility API: reading payloads belongs to repositories
(TASK-260715-1opnb2), and identity/cursor versioning already lives in
`gramdrive-model` (TASK-260715-1qz1g5). Finding 1 is the real constraint on
that path and is recorded rather than pre-empted.

## Handoff

- `RepairKind::RebuildProjection` is the interface to startup reconciliation
  (TASK-260715-21clwh) and the repair entrypoint (TASK-260715-1nuhxj):
  markers are raised here, cleared there.
- `StateError::MigrationRequired` is now unreachable in a correct build (the
  const assertion covers it) and deliberately kept as a typed error rather
  than an assumption. Tested via a deliberately gapped registry.
