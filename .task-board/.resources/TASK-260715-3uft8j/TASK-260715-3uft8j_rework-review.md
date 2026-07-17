# TASK-260715-3uft8j — Rework review: deterministic fake DriveSource

**Verdict: accepted → `done`.** All six requested items are fixed, and the one that
mattered — the recorder misattribution — is fixed correctly, verified by my own repro
and by mutation rather than by trusting green tests. `make check` 8/8.

## Verification — nothing taken on trust

Every claim in `TASK-260715-3uft8j_rework-results.md` that is measurable, I measured.

| Claim | Verdict |
|---|---|
| `make check` 8/8 | **CONFIRMED** — re-run clean (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts) |
| 151 tests (76 unit + 68 integration + 7 doc) | **CONFIRMED** — exact, counted off the mutant run (75+1 / 67+1 / 7) |
| The reviewer's repro now prints `Ok` | **CONFIRMED** — I rebuilt my *own* repro from the attached artifact, unmodified, against the fixed crate as a path dep (public API only). Prints `Ok` where it printed `Cancelled { delivered: 0 }`. The defect is gone through the same door it came in |
| Mutation fails *exactly* the two new tests | **CONFIRMED by my own mutation** — stripped the epoch guard from `settle()` in an isolated copy: `record::tests::a_call_cleared_while_in_flight_cannot_settle_a_later_call` (unit) and `a_call_still_in_flight_across_a_clear_cannot_rewrite_a_later_one` (integration) fail; **all other 142 pass**. Both tests are load-bearing, one per layer |
| `grep -c 'Call::Fetch'` → 3 | **CONFIRMED** (actually 5 — undercounted in their favour, was 0) |
| Blast radius testkit-internal | **CONFIRMED** — no crate references `gramdrive-testkit` in any `Cargo.toml`; the `Outcome::Failed` shape change reaches nothing outside the crate |

## The fix itself (finding 1) — right approach, right reasoning

Epoch on `Log`, bumped by `clear()`, remembered by each `CallGuard`, checked in
`settle()`. A stale guard settles **nothing**.

Endorsing the choice of epoch over monotonic ids, and the reasoning given for it:
ids would have leaked "every call ever" into `seq`, a field whose doc promises
positional-from-zero. The fix preserves the documented meaning instead of quietly
widening it. The "writing anything at all would be a fabrication" semantic is the
correct one for a crate whose product is evidence — a cleared entry is discarded on
purpose, and resurrecting it would be a second lie in place of the first.

The honesty note on the other two clear-tests is appreciated and correct: the
"silent loss" mode *was* already safe by accident (`get_mut` → `None`), those tests
pass under mutation, and they are claimed as intent-documentation rather than
regression coverage. That is the accurate description, and volunteering it is the
behaviour I want from an implementer.

## Findings 2–6

- **2 — Fetch arguments:** `every_call_is_recorded_in_order_with_its_arguments` now
  `assert_eq!`s the whole `Vec<Call>` including `Fetch { item, version, range }` with
  a non-trivial `4..12` range. A widened range or a dropped pin now fails. AC gap closed.
- **3 — `fault.rs` doc:** now states the every-matching-fault-advances rule and, better
  than asked, explains *why* it composes (the alternative makes `Nth(n)` mean "the n-th
  call this fault happened to win"). Matches `fake.rs::gate` as implemented. Verified
  line by line against the impl.
- **4 — full-scenario test:** the docstring was not weakened; the test was made to earn
  it. Real cancellation now exists (a `Children`/`chat_id(100)` `.delay(2)` fault polled
  to `Pending` then dropped → `Cancelled { delivered: 0 }`) — and note the delay is now
  *observable*, which is what creates the cancellation point rather than merely being
  scripted next to one. All 10 calls asserted with full arguments, all 10 outcomes
  asserted individually including the previously untouched 4/5/6. This is now what I
  pointed at as the standard, not a sample of it.
- **5 — Changes/Thumbnail faults:** both added; all six `Operation` variants now have a
  fault played on them. `a_fault_can_break_the_change_feed` goes past the ask by pinning
  that the same cursor still serves the retry — the property a sync loop actually needs.
  The thumbnail test distinguishing a scripted *failure* from a scripted *absent*
  thumbnail is the right instinct for that operation.
- **6 — assertions:** all seven tightened as requested. `is_err()` → scripted variant per
  attempt; chunking now `assert_eq!(sizes, vec![10,10,10,10,10,6])` (vacuity gone);
  `delivered == 20` exactly with the poll arithmetic written down; the race asserts the
  **record** with the sink kept as corroboration; the duplicate deleted with a comment
  saying why, so it does not get re-added.

## Finding 7 — decided well, and decided at the right time

`Failed { error, delivered }` was a decision, not rework, and taking it *now* rather
than after TASK-260715-3e8q4m builds on the type is the correct call — this is the
cheapest this change will ever be. The AC says "assert side effects"; a race that moved
8 bytes was previously visible only through the sink, and
`a_race_records_the_bytes_it_delivered` was papering over that under a name that
promised the record. Fixing the type instead of the test name was the right direction.

Endorsing both refusals as well:
- **no `delivered()` accessor** — it would have to answer `0` for a *successful* fetch
  that delivered its whole range, which is a lossy answer to the question it names.
- **`Ok` carries no count** — a success delivered its whole range by contract, and the
  range is already in `Call::Fetch`. Not storing a derivable value is right.

## DoD

Deterministic fake, recording, testkit-resident and dependency-free, gates green, tests
written and passing, lint clean, artifacts on the board, logbook carries both the bug
(1444) and the fix (1458) with the mutation evidence. All met.

## Note for TASK-260715-3e8q4m

The `Outcome` shape (`Failed { error, delivered }` / `Cancelled { delivered }`) is now
settled and is the intended stable surface for the conformance suite. Build on it.
