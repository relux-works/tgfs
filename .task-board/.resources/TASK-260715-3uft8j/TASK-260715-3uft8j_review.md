# TASK-260715-3uft8j — Review: deterministic fake DriveSource

**Verdict: changes requested → `to-dev`.** One confirmed correctness defect in the
recording layer (reproduced through the public API), plus an AC-coverage gap on the
"assert requests" half. Everything else is strong work and the design decisions are
right; the rework is small and local.

## What I verified independently (not taken on trust)

| Claim in `TASK-260715-3uft8j_results.md` | Verdict |
|---|---|
| `make check` 8/8 | **CONFIRMED** — re-run clean, all 8 steps |
| 144 tests (72 unit + 65 integration + 7 doc) | **CONFIRMED** — exact |
| `result_large_err` exempt: `ScriptError`=168, `SourceScript`=288, `Result`=288 | **CONFIRMED** — measured 168 / 288 / 288 exactly. The "error rides free" argument is sound |
| Removing `wake_by_ref` fails *only* the waker-respecting test | **CONFIRMED by mutation** — mutated a throwaway copy: 1 failure (`every_yield_wakes_itself_so_a_real_runtime_can_drive_the_fake`), other 64 integration + 72 unit tests pass. The test is genuinely load-bearing |
| SplitMix64 / FNV-1a pinned outputs | **CONFIRMED** — canonical published values |
| Architecture boundary (model + source only, no runtime, dev-dep only) | **CONFIRMED** — no product crate depends on the testkit; zero new deps |
| Replay determinism tests compare two independent runs | **CONFIRMED** — fresh script + fresh source per run, and `assert_ne!` on a different seed keeps it non-vacuous |
| `a_full_scripted_scenario_...` "asserts the exact 9-call interaction log" | **OVERSTATED** — see finding 4 |

The implementation notes were honest on every measurable claim. The one overstatement
is finding 4.

## Findings

### 1. [correctness] `clear_interactions()` misattributes outcomes when any call is in flight

`Recorder::begin` assigns `seq = log.len()`; `Recorder::settle` writes `log[seq]`.
`clear()` (`src/record.rs:194`) empties the log without invalidating the `seq` held by
live `CallGuard`s, so indices are reused. A guard outstanding across a clear settles
whatever entry now occupies its old index.

Reproduced through the public API only (`.temp/TASK-260715-3uft8j/repro_clear_interactions.rs`):

```
== after a SUCCESSFUL root() call, log says ==
  Interaction { seq: 0, call: Root, outcome: Ok }
== after dropping an unrelated in-flight fetch, the SAME root() entry says ==
  Interaction { seq: 0, call: Root, outcome: Cancelled { delivered: 0 } }
root() actually returned: Ok(Account)
```

Two silent failure modes:
1. **Misattribution** (above): a successful call's outcome is overwritten by an
   unrelated dropped future.
2. **Silent loss**: if the log is shorter than the stale `seq`, `settle`'s
   `log.get_mut(seq)` returns `None` and the outcome vanishes — a cancellation test
   that cleared first records *nothing*.

Why this matters more here than it would elsewhere: this module's entire product is
trustworthy evidence, and the wrong answer it produces is a *plausible* one
(`Cancelled` is an expected outcome in this suite), so it misleads rather than
crashes. The downstream consumer is the conformance suite (TASK-260715-3e8q4m) —
"the harness silently lied about what the source did" is the one failure mode a
conformance fixture must not have.

Not caught because `interactions_can_be_cleared_between_phases`
(`tests/fake_source.rs:1336`) only clears when every call has already settled — the
safe path.

Fix is small and local: give the log a generation/epoch that `clear()` bumps and the
guard carries, or key entries by a monotonic id instead of a positional index. Please
add a test that clears with a call in flight.

### 2. [test-coverage] `Call::Fetch` arguments are never asserted anywhere — AC gap

`grep -c 'Call::Fetch' tests/fake_source.rs` → **0**. The AC is "assert requests,
cancellation, and side effects". `Fetch` is the call whose arguments are richest —
item + pinned version + byte range (`src/record.rs:56-60`) — and the recorded
arguments are never compared. `every_call_is_recorded_in_order_with_its_arguments`
(`tests/fake_source.rs:1240`) does an exact `assert_eq!` for the other five calls and
omits fetch.

### 3. [docs] `fault.rs` module doc states the opposite of the implemented behavior

`src/fault.rs:34-36`: *"A second matching fault ... its counter does not advance,
because it never matched."*

`src/fake.rs:225-238` increments the counter for **every** fault that `matches(...)`,
firing or not — and `src/fake.rs:214-220` documents exactly that. Verified empirically:
two `Fetch` faults at `Nth(1)`/`Nth(2)` fire on calls 1 and 2 respectively
(`FIRST-FAULT`, `SECOND-FAULT`, then `Ok`), which only happens if the second's counter
advanced on call 1.

**The behavior is right — the counting rule that composes is the one implemented.**
The module doc is the defect, and it is the doc a fixture author reads to reason about
multi-fault scripts.

### 4. [docs] `results.md` overstates the end-to-end scenario test

Claimed: *"asserts the exact 9-call interaction log"*. Actually
(`tests/fake_source.rs:1580-1624`) it builds `summary` by **discarding the arguments**
(`.map(|entry| (entry.call.operation(), &entry.outcome))`) and then asserts:
- `summary.len() == 9` (a count, not the log)
- operation for **2 of 9** entries (`[0]`, `[1]`)
- outcome for **6 of 9** — indices **4, 5, 6 are never touched** (`latest_cursor`,
  `changes`, `changes` go entirely unasserted)
- no call arguments compared at all

The failure-count check is the only global pin. `every_call_is_recorded_in_order_with_its_arguments`
is what an exact log assertion looks like; this test isn't that.

Also, the test's own docstring (`:1484-1486`) claims the scenario covers "delays,
failures, version races **and cancellation**" — there is **no cancellation** in it (no
dropped future, no `Outcome::Cancelled`), and the `.delay(1)` at `:1501` is
unobservable because every call goes through `exec::drive`, which polls to completion.
Either add those or correct the claim; a docstring that overstates coverage is how a
gap survives review.

### 5. [test-coverage] No fault is ever played on `Changes` or `Thumbnail`

Faults are scripted only on `Root` (6), `Fetch` (6), `Children` (3), `LatestCursor` (3)
— never `Operation::Changes`, never `Operation::Thumbnail`. `src/script.rs:839-841`
*builds* such faults but nothing ever plays one. Both are gated operations in
`fake.rs`; neither has a delay/failure exercised.

### 6. [test-quality] Assertions weaker than the fixture they test

- `a_bounded_fault_recovers_after_its_run` (`:934`), `a_source_can_break_and_stay_broken`
  (`:954`), `an_item_filter_targets_only_that_item` (`:1005`) script a *specific* error
  then assert only `is_err()` — they'd pass if the fake returned `NotFound` instead of
  the scripted fault.
- `fixed_chunking_cuts_at_stated_boundaries` (`:716`): `sizes.iter().take(n).all(...)`
  is vacuously true if the iterator is shorter than `n`; no `assert_eq!(sizes.len(), ..)`,
  so `[10,10,10,10,10,3,3]` passes.
- `dropping_a_fetch_mid_delivery_records_how_far_it_got` (`:1110`) asserts
  `0 < delivered < 56` when the value is deterministic (`Fixed(4)`, 5 polls → exactly
  20). Loose bounds in a crate whose premise is exact poll accounting.
- `a_version_conflict_asks_for_a_refresh` (`:685`) touches no fake code at all — it is
  a verbatim duplicate of `gramdrive-source/src/error.rs:265-271`.
- `a_race_records_the_bytes_it_delivered` (`:849`) asserts the *sink's* count, not the
  record's; the name claims otherwise.

### 7. [design, non-blocking] `Outcome::Failed` carries no `delivered` count

Only `Cancelled` does (`src/record.rs:98-117`). A version race or sink-stop that moved
8 bytes reports that via the sink but never via `interactions()`. If "assert side
effects" is meant to be answerable from the interaction log alone, this is a gap in the
type, not the tests. Worth a decision before the conformance suite builds on it —
flagging, not requiring.

## Design decisions reviewed — all endorsed

- **A delay is a count of yields, not a `Duration`** — correct, and the reasoning is
  the right reasoning. A clock-sleeping fake would be non-reproducible by construction
  and would force a runtime on every consumer. Consequence (zero new deps) is real.
- **Page tokens name their revision; continuations across a revision are refused** —
  correct. Stricter than SYNC-003 requires, but refusing is always contract-legal and
  splicing never is. Right call for a fixture whose job is catching that exact bug.
- **PRNG/hash written out rather than depended on** — correct. `rand` and
  `DefaultHasher` carry no output-stability guarantee; pinned constants make a change
  fail the suite instead of silently re-cutting every delivery.
- **`build()` validates up front** — correct, and it is what lets every later
  `SourceError` be contract- or script-attributable.
- **Lint exemptions** — argued from measurement and the measurements check out. Keep.

## What to do

Address 1 and 2; correct 3 and 4; 5 and 6 are cheap while in there. 7 is a decision,
not rework. Then back to `to-review`.
