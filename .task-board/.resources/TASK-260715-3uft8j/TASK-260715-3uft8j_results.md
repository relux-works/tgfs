# TASK-260715-3uft8j — Deterministic fake DriveSource

**Status:** ready for review · **Gates:** `make check` 8/8 · **Tests:** 144 (72 unit + 65 integration + 7 doc)

Built the deterministic fake `DriveSource` in `gramdrive-testkit`, playing a
declarative script against the real contract from TASK-260715-1j4ij3.

## What landed

| File | Contents |
|---|---|
| `src/script.rs` | `SourceScript` / `ScriptBuilder` / `ScriptError` / `ChunkPlan` — the declarative backend, validated and frozen by `build()` |
| `src/fake.rs` | `FakeSource` — plays a script: paging, change feed, ranged fetch, thumbnails, faults |
| `src/fault.rs` | `Fault` / `Operation` / `Occurrence` / `Effect` — scripted delays, failures, version races |
| `src/record.rs` | `Interaction` / `Call` / `Outcome` — the recording, cancellation included |
| `src/sink.rs` | `RecordingSink` — a `ContentSink` that verifies the delivery contract while collecting |
| `src/exec.rs` | `drive` / `try_drive` / `poll_n` — single-threaded executor, no runtime dependency |
| `src/fixture.rs` | Identity and item constructors for writing scripts |
| `src/tree.rs`, `src/rng.rs` | Internal: the revision tree, and pinned deterministic PRNG/hash |
| `tests/fake_source.rs` | 65-test behavioral suite, written as an integration test |

## Acceptance criteria

> *Tests can reproduce every configured event by seed/script and assert requests, cancellation, and side effects.*

| Required | Where |
|---|---|
| Scripted snapshots | `every_page_of_one_enumeration_reports_the_same_snapshot` |
| Paged listings | `enumeration_covers_every_child_exactly_once`, page sizes 1 / N / oversized |
| Change feeds | `the_feed_serves_one_batch_per_page_in_order`, cursor round-trip, 4 rejection paths |
| Ranged reads | full range, partial range, past-extent, directory, restricted, not-found |
| Injectable delays | `a_delay_holds_the_call_pending_for_exactly_its_yields` |
| Failures | `Nth` / `Always` / `FirstN` / `FromNth`, item filters, flood-wait backoff preserved |
| Version races | `a_version_race_cuts_delivery_and_conflicts`, race-at-zero, stale-pin |
| Cancellation points | drop mid-fetch, drop mid-delay, in-band sink `Stop`, drop unpolled |
| Reproducible by seed/script | `an_identical_script_replays_identically`, `seeded_chunking_replays_identically_for_one_seed` |
| Request recording | `every_call_is_recorded_in_order_with_its_arguments` |
| Cancellation propagation | `dropping_a_fetch_mid_delivery_records_how_far_it_got` |
| Side effects | `Outcome::Cancelled { delivered }` / `Outcome::Failed { delivered }`; `RecordingSink::chunks`/`bytes`/`progress` |
| Usable from any crate | The suite is an integration test — it links the crate as a downstream consumer does, through the public API only |

`a_full_scripted_scenario_reaches_every_configured_event` drives all of it from
one script end to end and asserts the exact 10-call interaction log.

> **Corrected in rework** (see `TASK-260715-3uft8j_rework-results.md`). As
> originally written this claim was an overstatement, caught by review: the test
> asserted a 9-entry *count*, 2 of 9 operations and 6 of 9 outcomes, with no
> arguments compared, and its docstring claimed cancellation coverage it did not
> have. The test now asserts the full 10-call log with arguments and every
> outcome, and includes a real cancellation. The claim holds as of the rework;
> it did not hold when first written.

## Design decisions worth review attention

**1. A delay is a count of yields, not a `Duration`.** This is the central
call. A fake that slept on a real clock would be non-reproducible by
construction — a deterministic fixture whose result depends on host load — and
would force an async runtime dependency on every consumer. Nothing the delay
exists to test is about elapsed time; what tests need is what an `await`
*provides*: a point to cancel at, to interleave at, to observe an in-flight
call at. `delay(3)` means "return `Pending` three times". Wall-clock time
reaches the contract only where the contract names it (the `Duration` inside
`SourceError::RateLimited`, which is data the caller reads, not time anyone
waits).

Consequence: `gramdrive-testkit` gained **no new dependencies**. It has no
runtime, no `rand`, no clock.

**2. Every yield wakes itself before parking** (`yield_now` pattern), so the
fake is drivable by the bundled `exec` *and* by the engine's tokio. `exec`'s
noop-waker loop structurally cannot catch a missing wake — it re-polls
regardless — so `every_yield_wakes_itself_so_a_real_runtime_can_drive_the_fake`
drives the fake with an executor that polls only when actually woken. I
verified this test fails if the `wake_by_ref` is removed; the other 64 tests
pass without it. Without this test the claim would have been unfalsified.

**3. A page token names the revision it was minted at**, and a continuation
presented at another revision is refused with `CursorRejected`. Stricter than
SYNC-003 requires — a source *may* keep serving an old snapshot — and
deliberate: the alternative is a fake that splices two states into one
enumeration and returns a listing with a duplicate or a hole, which is the
exact contract failure the conformance suite exists to catch. Refusing is
always contract-legal; splicing never is.

**4. `build()` validates the whole script up front** — one root, a tree that is
a tree, every batch applying cleanly at its own revision, every fetchable file
having bytes matching its declared size at every revision it exists at. Past
`build`, every `SourceError` the fake produces is one the contract specifies or
the script asked for, never the fake improvising around a malformed fixture.

**5. The PRNG and hash are written out, not depended on.** `rand` carries no
cross-version output-stability guarantee and `DefaultHasher` explicitly
reserves the right to change. A fixture whose chunk boundaries move when a
dependency is bumped is a flake with a seed field. SplitMix64 and FNV-1a are a
handful of frozen constants; `rng::tests` pins concrete outputs so a change
fails the suite instead of silently re-cutting every scripted delivery.

## Two lint exemptions, both argued rather than assumed

- **`clippy::result_large_err`** (module-level in `script.rs` / `tree.rs`).
  `ScriptError` is 168 bytes (three variants name two `ItemId`s; an `ItemId` is
  80). The lint is about a fat `Err` taxing a hot success path, and neither half
  applies: measured, `Result<SourceScript, ScriptError>` is **288 bytes —
  exactly `size_of::<SourceScript>()`**, so the error rides free inside the `Ok`
  footprint; and these functions run once per scripted item at fixture-build
  time. Both remedies cost more than they save (boxing adds an allocation in
  front of a free value; shrinking means storing identities as strings, losing
  the typed `ItemId` a caller can assert against).
- **`clippy::expect_used` / `panic`** (`tests/fake_source.rs`, and
  `exec::drive`). `clippy.toml` already exempts test code on the grounds that a
  panicking test is just a failing test; that exemption keys on `#[test]` fns
  and `#[cfg(test)]` modules, and integration-test module-level helpers are
  neither. The rationale still holds in full — this crate is a dev-dependency by
  architecture rule and links into no product artifact.

## Bugs caught during development

- `Tree::upsert` accepted a change moving an item to `parent: None`, silently
  creating a second root — `link` cannot tell *the* root from *a* parentless
  item. Fixed with an explicit rootness check (`RootReparented`); rootness is
  structural and cannot be edited.
- Two invented pinned constants (an FNV-1a value, and a poll-count assertion)
  were wrong; both replaced with measured values. Worth noting for review: the
  test suite caught both, which is the point of pinning them.

## Verification

```
cargo test -p gramdrive-testkit   72 unit + 65 integration + 7 doc, all passing
make check                        8/8 (toolchain, format, lint, test, architecture,
                                  supply-chain, traceability, scripts)
```

Architecture gate confirms the boundary holds: internal deps still
`gramdrive-model` + `gramdrive-source` only, no platform code, testkit still
dev-dependency-only.

## Notes for the conformance suite (TASK-260715-3e8q4m)

- `RecordingSink` already enforces SYNC-046 on every delivery — the suite gets
  that check for free against any implementation.
- `fixture` is deliberately lean (identities + item constructors). Shared
  fixture *trees* and the PLAT-021 filename fixtures are still unclaimed and
  belong to that task.
- `exec::poll_n` is the cancellation primitive: drive to a point, drop, read
  `interactions()`.
