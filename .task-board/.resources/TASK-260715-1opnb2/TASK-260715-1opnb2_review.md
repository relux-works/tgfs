# TASK-260715-1opnb2 — Review: transactional state repositories

**Verdict: ACCEPTED → done.**

Reviewed the full diff: `crates/gramdrive-state/src/repo/` (11 modules, ~4.1k lines),
error/lib/store changes, 5 new integration-test files (~2.1k lines), README
sections, fixtures. Every repo module read in full; tests for the AC paths read
in full.

## AC verification

| AC | Evidence | Verified |
|---|---|---|
| Atomic cursor application | `repo_changes.rs::cursor_commits_atomically_with_applied_changes`, `a_failed_cursor_write_rolls_back_the_whole_batch` (failure path: batch applied, cursor rejected with `CursorOutOfScope`, drop rolls back both) | read + green |
| Idempotent replay | `replaying_a_batch_applies_nothing` (exact replay recognized whole, event log byte-identical), stale-revision and post-deletion skips in `edits_append_new_revisions...`, `deletions_tombstone_and_never_imply_or_resurrect` | read + green |
| Version conflict | item CAS (`item_content_updates_are_compare_and_set`), transfer promotion re-check inside the promoting txn (`promotion_rechecks_the_content_version_it_pinned`, SYNC-042), watermark regression, cursor epoch rejection | green |
| Concurrent readers/writers (WAL) | `repo_concurrency.rs`: snapshot stability across a foreign commit, two threads with separate connections racing `claim_next_transfer` behind a `Barrier` (no double-claim), reader asserting cursor-never-ahead-of-state across 20 batches | read + green |
| Multi-process safety documented | README "Repositories" + "Multi-process safety" sections; two connections exercise the same file-based locking two processes would | read |
| No SQL/FFI leakage | public surface speaks `gramdrive-model` types + record types only; enum strings, range JSON, cursor text all mapped both ways; unknown stored text is typed `CorruptRow`, never coerced. `StateError::Sqlite(rusqlite::Error)` predates this task (opaque error source, not an operational type). Architecture gate green. | read |

## Gates (rerun during review)

- `cargo test -p gramdrive-state` — green (all suites incl. 36 new repo tests).
- `make check` — **7/8**: `test` step failed in `gramdrive-model
  --test naming_properties::sanitize_is_idempotent`. **Unrelated to this task**
  (diff touches `gramdrive-state` only): a fresh random proptest counterexample
  against the TASK-260715-1ffbkg sanitizer, deterministic once the seed
  replays. Filed as **BUG-260717-3rr59f** under STORY-260715-3qxar5.
  The implementer's 8/8 run was legitimate — the counterexample had not been
  drawn yet. The regressions file in the working tree was restored to its
  handed-off state to keep this review read-only; deterministic repro seed:

  ```
  cc 5ee1185dfc7b144b61724297cc5303bcd6b4eeadd4f261bd666596f2bd27723f # shrinks to raw = "/é///\u{200c}\u{200c}\u{301}//\u{301}\u{200c}/////\u{301}\u{200c}///𐀀/\u{301}ࠀaéé\u{301}\u{200c}/é¡é/\u{200c}///\u{200c}/\u{200c}/\u{200c}/\u{200c}////\u{200c}/\u{301}////\u{301}/\u{200c}é\u{200c}//\u{200c}\u{301}\u{301}\u{200c}//////\u{200c}ࠀ/é😀/\u{200c}a\u{301}\u{301}///\u{202f}é\u{200c}/////\u{301}//\u{301}//\u{301}//é\u{200c}///////ࠀ\u{301}///\u{301}.\u{200c}\u{200c}<LM N4L\u{200c}`|家b?\u{34468}\u{200d}\u{200d}\u{104394}..|\u{200c}\u{200c}?<L😀|N\u{200c}", kind = File
  ```

  (append to `crates/gramdrive-model/tests/naming_properties.proptest-regressions`,
  then `cargo test -p gramdrive-model --test naming_properties sanitize_is_idempotent`).

## Architecture fit

Solid. The layer consistently enforces invariants *in the layer* rather than
trusting callers: item identity columns derived from the `ItemId`; epoch moves
only through `bump_namespace`; eviction eligibility inside the DELETE;
promotion re-checks inside the promoting transaction; watermark regression a
typed refusal; publish re-checks the event log in its own transaction.
`&mut self` transaction API makes one-connection-one-transaction
compile-enforced; multi-process concurrency is separate connections, which is
the app + FP extension shape. SYNC-022 atomicity as composition (no special
API) is the right call. Hand-rolled range codec is justified by POL-6 and
strictly tested. Error vocabulary is precise and каждый variant documented with
its rationale.

## Non-blocking observations (recorded, no rework requested)

1. **`pin_item` origin overwrite is directionally unguarded**
   (`repo/cache.rs:496-502`): `DO UPDATE SET origin = excluded.origin` lets an
   ArchiveMode re-pin silently downgrade a User pin; Archive-Mode teardown
   (`pins(Some(ArchiveMode))` → unpin) would then release user intent — the
   POL-2 outcome the docstring promises cannot happen. The tested direction
   (ArchiveMode → User upgrade) holds. The engine (STORY-260715-2hs8cf) owns
   pin orchestration and can check first, but a directional upsert (user wins)
   would close it in the layer at zero expressiveness cost (explicit
   unpin+pin still downgrades). Logged in LOGBOOK for the
   cache-quota-and-eviction task.
2. **README overstates the cursor read-side check**: "scope-checked both ways
   against the account's *current* epoch" — the read path
   (`cursors.rs::cursor`) verifies against the caller-passed scope, not the
   account row; only `put_cursor` checks the current epoch. In practice callers
   obtain the scope via `current_scope()` under the same snapshot, and the
   write-side check is the one that prevents mis-apply, so this is a wording
   nit, not a hole.
3. **Replay after a Mirror-mode payload purge** would re-append a purged
   payload as an `edited` event (`apply_revision` sees `same_payload = false`,
   equal revision times pass the stale guard). Purge is explicitly out of this
   task's scope (retention flow); flagging it so the retention task considers
   the interplay.

## Checklist disposition

All DoD items verified: typed layer ✓, AC tests ✓, multi-process docs+tests ✓,
lint/format/architecture/supply-chain/traceability green ✓, task-scoped outcome
artifacts ✓, logbook entries ✓ (implementer's 1745–1748 + review entries).
