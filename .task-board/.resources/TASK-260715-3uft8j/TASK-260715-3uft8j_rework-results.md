# TASK-260715-3uft8j — Rework results

Rework against `TASK-260715-3uft8j_review.md` (changes requested). Design
decisions were endorsed by review and are untouched. All six items addressed,
plus a decision on the non-blocking finding 7.

**`make check`: 8/8.** Testkit tests: **151** (76 unit + 68 integration + 7 doc),
up from 144. Workspace: 335 passing.

---

## 1. [correctness] `clear_interactions()` misattribution — fixed

**Fix:** the log now carries an epoch (`record.rs`, `struct Log { entries, epoch }`).
`clear()` bumps it; each `CallGuard` remembers the epoch it began in; `settle()`
returns early unless the epochs match. A guard from a cleared epoch settles
nothing — its entry was discarded deliberately, so writing anything at all
would be a fabrication.

Chose epoch over monotonic ids: it fixes the defect without changing what `seq`
means. `seq` stays positional-from-zero, so a test that clears between phases
still reads `seq: 0` for the first call it cares about. Documented on
`Interaction::seq`, `Recorder::clear`, `FakeSource::clear_interactions`, and in
the `record.rs` module docs.

**Verified two ways, not just "tests pass":**

- **The reviewer's own repro, run unchanged** against the fixed crate
  (`.temp/TASK-260715-3uft8j/repro/`, testkit as a path dep, public API only):

  ```
  == after a SUCCESSFUL root() call, log says ==
    Interaction { seq: 0, call: Root, outcome: Ok }
  == after dropping the unrelated in-flight fetch, the SAME root() entry says ==
    Interaction { seq: 0, call: Root, outcome: Ok }     <-- was Cancelled { delivered: 0 }
  ```

- **Mutation**: commenting out the epoch check in `settle()` fails exactly the
  two new tests written for it, one per layer, and nothing else:
  - `record::tests::a_call_cleared_while_in_flight_cannot_settle_a_later_call` (unit)
  - `a_call_still_in_flight_across_a_clear_cannot_rewrite_a_later_one` (integration —
    the reviewer's repro as a test)

  Restored afterwards; `make check` re-run clean.

**Honest note on the other two clear-tests.** `a_call_cleared_while_in_flight_settles_nothing_into_an_empty_log`
and `a_call_cleared_while_in_flight_is_not_resurrected_by_its_own_drop` pass
under the mutation too — the "silent loss" mode was already safe by accident
(`get_mut` on a short log returns `None`). They are kept as intent-documentation
against a future re-implementation, not claimed as regression coverage.

## 2. [test-coverage] `Call::Fetch` arguments — asserted

`every_call_is_recorded_in_order_with_its_arguments` now drives a fetch and
`assert_eq!`s the whole `Vec<Call>` including `Call::Fetch { request: { item,
version, range } }`, with a non-trivial range (`4..12`) so a caller that widened
its range or dropped its pin fails the assert. `grep -c 'Call::Fetch'
tests/fake_source.rs` → 3 (was 0).

## 3. [docs] `fault.rs` counting rule — corrected

The doc said a second matching fault's "counter does not advance, because it
never matched". The implementation advances **every** matching fault's counter.
The implementation was right; the doc is rewritten to say so, and to say *why*
that rule is the one that composes: under the other rule `Nth(n)` would silently
mean "the n-th call this fault happened to win", which depends on what else is
in the script. `faults_on_different_occurrences_compose` is the test that pins it.

## 4. [docs] Full-scenario test — made to match its docstring

The docstring claimed delays, failures, version races **and cancellation**; the
test had no cancellation, its `.delay(1)` was unobservable through `exec::drive`,
and it asserted 2/9 operations, 6/9 outcomes (indices 4/5/6 untouched), zero
arguments. Rather than weaken the docstring, the test now earns it:

- A new `Children`/`chat_id(100)` fault with `.delay(2)` and no failure, dropped
  mid-delay: `poll_n(.., 2) == Pending` (the delay is now **observable**), then
  `drop` → `Outcome::Cancelled { delivered: 0 }`. Real cancellation, and the
  delay is what creates the cancellation point.
- **All 10 calls** asserted exactly via `assert_eq!(source.calls(), vec![..])` —
  full arguments, not operations.
- **All 10 outcomes** asserted individually; the previously untouched
  `latest_cursor` / `changes` / `changes` entries included.

The `results.md` overstatement ("asserts the exact 9-call interaction log") is
now simply true of the reworked test (10 calls).

## 5. [test-coverage] Faults on `Changes` and `Thumbnail` — added

- `a_fault_can_break_the_change_feed`: `Changes` fails once and recovers, and
  **the same cursor still serves the retry** — the property a sync loop depends
  on. Asserts the scripted error type and that the failure is on the record.
- `a_fault_can_delay_and_break_a_thumbnail`: `Thumbnail` with `delay(2)` (polled
  to `Pending`) then a scripted `Restricted`, plus the case that matters for this
  operation specifically — a scripted *failure* stays distinguishable from a
  scripted *absent* thumbnail (`Ok(None)` for an unfiltered directory).

Every `Operation` variant now has a fault played on it.

## 6. [test-quality] Assertions tightened

| Test | Was | Now |
|---|---|---|
| `a_bounded_fault_recovers_after_its_run` | `is_err()` | `matches!(.., Unavailable)` per attempt |
| `a_source_can_break_and_stay_broken` | `is_err()` | `matches!(.., Internal)` per attempt |
| `an_item_filter_targets_only_that_item` | `is_err()` | `matches!(.., Restricted)` |
| `fixed_chunking_cuts_at_stated_boundaries` | vacuous `take(n).all(..)` | `assert_eq!(sizes, vec![10,10,10,10,10,6])` — the cut is determined, so it is stated |
| `dropping_a_fetch_mid_delivery_records_how_far_it_got` | `0 < delivered < 56` | `delivered == 20` exactly |
| `a_race_records_the_bytes_it_delivered` | asserted the *sink* | asserts the **record** (`Failed { delivered: 8 }`), sink kept as corroboration |
| `a_version_conflict_asks_for_a_refresh` | duplicate of `gramdrive-source/src/error.rs` | **deleted**, with a comment saying why so it is not re-added |

## 7. [design] `Outcome::Failed` delivered count — decided: added

Review flagged this as a decision, not rework. **Decided to make the change now**,
before the conformance suite (TASK-260715-3e8q4m) builds on the type:

```rust
Failed { error: SourceError, delivered: u64 }   // was Failed(SourceError)
```

Rationale: the AC is "assert requests, cancellation **and side effects**". A
version race or sink-stop that moved 8 bytes is a side effect that was only
visible through the sink, never through `interactions()`. That is a gap in the
type, and `a_race_records_the_bytes_it_delivered` was papering over it by
asserting the sink under a name that promised the record. Cheaper to fix the
type now than after a downstream suite depends on it.

**Deliberately did not add a `delivered()` accessor.** It would have to return
`0` for `Outcome::Ok` — including a *successful* fetch that delivered its whole
range — which is a lossy answer to the exact question the accessor names. Callers
match the variant instead; the two variants that can be partial say so explicitly.

`Outcome::Ok` deliberately does **not** carry a count either: a successful fetch
delivered its whole range by contract, and the range is already in `Call::Fetch`.

Blast radius: testkit-internal. No product crate depends on the testkit
(dev-dep only, re-verified), so no consumer outside `tests/fake_source.rs`
touches `Outcome`. README's type table updated.

---

## Files touched

- `src/record.rs` — epoch-keyed log; `Failed { error, delivered }`; module docs; 4 new unit tests
- `src/fake.rs` — `clear_interactions` in-flight semantics documented
- `src/fault.rs` — counting-rule doc corrected
- `tests/fake_source.rs` — 4 tests added, 1 deleted, 8 tightened, full-scenario reworked
- `README.md` — `Outcome` type table row

## Not done / limits

- No new dependencies; no product code touched; architecture boundary unchanged.
- The fake still has no load, threads, or clock — unchanged and by design.
