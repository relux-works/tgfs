# gramdrive-testkit

Test support shared across the core: the deterministic fake `DriveSource`,
the source conformance suite, and shared fixture trees including
cross-platform filename fixtures (PLAT-021). Product crates may use it only
as a `dev-dependency` — it never ships in a product artifact.

## Ownership

STORY-260715-255sa3 (drive-source-contract), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-3uft8j (deterministic fake
source), TASK-260715-3e8q4m (conformance suite), TASK-260715-1ceq7h
(synthetic large-account fixtures).

## Dependencies

Internal: `gramdrive-model`, `gramdrive-source` (`gramdrive-render` allowed
for golden-file helpers). Platform-specific code: forbidden. No async
runtime, by design — see below. See `crates/README.md`.

## The deterministic fake source

`FakeSource` plays a `SourceScript` against the real `DriveSource`
contract: snapshot paging, a durable change feed, ranged delivery into a
sink, thumbnails, and the whole failure taxonomy.

```rust
let scope = fixture::scope();
let photo = fixture::attachment_id(scope, 100, 5, 0);

let script = SourceScript::builder(scope)
    .items([root_item, chat_item, photo_item])
    .content(&photo, ContentVersion::new("c1")?, *b"hello world")
    .batch([ItemChange::Upserted(edited_photo)])          // one revision
    .fault(
        Fault::on(Operation::Fetch)
            .occurrence(Occurrence::Nth(1))
            .delay(2)
            .fail(SourceError::Unavailable { detail: "link dropped".to_owned() }),
    )
    .build()?;

let source = FakeSource::new(script);
let mut sink = RecordingSink::new(range);
exec::drive(source.fetch(request, &mut sink))?;

assert_eq!(sink.bytes(), b"hello world");
assert_eq!(source.calls().len(), 1);
```

| Type | Purpose |
|---|---|
| `SourceScript` / `ScriptBuilder` | A backend written down: base tree, per-version file content, change batches, faults, seed, chunk plan. `build()` validates and freezes it |
| `FakeSource` | Plays a script. `advance()` applies the next batch; `interactions()` / `calls()` read back what was asked |
| `Fault` / `Occurrence` / `Effect` | Scripted delays, failures, and mid-fetch version races, targeted by operation, item, and call count |
| `Interaction` / `Call` / `Outcome` | The recording: arguments per call, and `Ok` / `Failed { error, delivered }` / `Cancelled { delivered }` per outcome. Both interrupted outcomes name the bytes that reached the sink, so a partial delivery is assertable from the log alone |
| `RecordingSink` | A `ContentSink` that folds every chunk through `FetchProgress`, so the delivery contract (SYNC-046) is checked in every test that uses it |
| `exec` | `drive` / `try_drive` / `poll_n` — a single-threaded executor, no runtime behind it |
| `fixture` | Identity and item constructors for writing scripts |

### Three design decisions worth knowing before using it

**A delay is a count of yields, not a `Duration`.** A fake that slept on a
real clock would be non-reproducible by construction, and would force a
runtime dependency on every consumer. Nothing the delay exists to test is
about elapsed time: what tests need is what an `await` provides — a point to
cancel at, a point to interleave at, a point where a fetch is provably still
in flight. `delay(3)` means "return `Pending` three times". Wall-clock time
reaches the contract only where the contract names it (the `Duration` inside
`SourceError::RateLimited`, which is data the caller reads, not time anyone
waits).

Every yield wakes itself before parking (the `yield_now` pattern), so the
fake is drivable by `exec` *and* by a real runtime such as the engine's
tokio. `tests/fake_source.rs` holds that claim to an executor that polls only
when actually woken — `exec`'s noop-waker loop cannot catch a missing wake,
and a real runtime catches it by hanging.

**The source changes only when the test says so.** `advance()` applies one
scripted batch; nothing moves on its own. A version race is `advance` between
a fetch's start and its next chunk; a rejected page token is `advance`
mid-enumeration. There is no scheduler to lose a race against.

**A page token names the revision it was minted at**, and a continuation
presented at another revision is refused with `CursorRejected`. Stricter than
SYNC-003 requires, deliberately: the alternative is a fake that splices two
states into one enumeration and returns a listing with a duplicate or a hole
— the exact failure the conformance suite exists to catch. Refusing is always
contract-legal; splicing never is. So a test wanting an uninterrupted
enumeration does not call `advance` mid-enumeration, and a test proving the
caller re-baselines correctly does exactly that.

### Reproducibility

Two runs of one script produce the same bytes, page boundaries, errors and
recordings, in the same order — asserted end to end by
`an_identical_script_replays_identically`. The only entropy is the script's
seed, which drives `ChunkPlan::Seeded` chunk boundaries; the generator and
hash are written out in `rng.rs` with pinned outputs, because `rand` and
`DefaultHasher` offer no cross-version output stability, and a fixture whose
chunk boundaries move when a dependency is bumped is a flake with a seed
field.

`ScriptBuilder::build()` validates up front: one root, a tree that is a tree,
every batch applying cleanly at its own revision, and every fetchable file
having bytes matching its declared size at every revision it exists at. Past
`build`, every `SourceError` the fake produces is one the contract specifies
or the script asked for — never the fake improvising around a malformed
fixture.

### What it does not do

It has no load, no threads and no clock, so it cannot answer "what happens
under contention" or "does this meet a latency budget" (NFR-020..022). Those
need a real backend and a real runtime. What it answers is every question
with a contractual answer, identically on every run.

## The conformance suite

One suite, run against every `DriveSource` implementation (SYNC-002,
NFR-002). The point of DEC-003's provider-neutral boundary is that the engine
holds a local TDLib source, a remote source, or a fake behind one `dyn
DriveSource` and does not care which — a promise worth exactly as much as the
shared test that checks it.

```rust
use gramdrive_testkit::conformance::{self, FakeHarness};

let report = conformance::run(&FakeHarness::new());
assert!(report.is_conformant(), "{report}");
```

### Running it against your backend

Implement `SourceHarness`: name the backend, declare which `Capability`s you
can stage, drive a future, and build `WORLD` on demand. Then call
`conformance::assert_conforms(&harness)` from a `#[test]`. The suite never
constructs your source, never reaches past the trait, and never learns your
page-token format; everything it knows about your world it learns from the
`Landmarks` you hand back. `conformance::FakeHarness` is the worked example —
about 250 lines, and the whole of what a backend owes the suite.

| Type | Purpose |
|---|---|
| `SourceHarness` | The seam: `name`, `supports`, `block_on`, `stage`. Generic, not a trait object — `block_on` is generic in its output because how futures are driven is the backend's business |
| `WORLD` / `WorldSpec` | The one world every case is staged against. Fixed, not configurable: a fixture that varies per backend gives results that cannot be compared across backends |
| `Landmarks` | Where the harness put each part of the world — identities only; the suite reads the rest through the contract |
| `Perturbation` / `Setup::arm` | Interference armed *before* the source goes live: a call that fails, throttles, takes its time, or loses a race |
| `Mutation` / `Setup::plan` / `Control` | Changes applied *while* it is live: a child appears or leaves, content moves on. Declared up front so a harness can prepare — the fake compiles the plan into change batches at build time |
| `Capability` | What a harness can stage. What it declines is **skipped**, never passed |
| `Report` / `Clause` / `CaseOutcome` | The verdict: per case, the clause it pins, the claim it makes, and what the source did instead |

### Two properties worth knowing

**Skipped is not passed.** No backend stages everything — a `tdjson` source
against a live account cannot conjure a flood wait on cue. `supports` lets a
harness decline, and the case is reported `Skipped`, printed in a list under
the pass count. `Report::is_conformant` means "broke nothing it was asked
about", which is not "correct", and `clauses_upheld` credits only clauses that
actually ran. Counting a skip as a pass would make the suite most flattering
to the backends that support least.

**A case asserts only what the contract mandates.** A conformance suite that
fails correct backends is worse than none, so each case pins the clause and
stops. It is why there is no case for "a page larger than the listing needs no
continuation" (`PageRequest` says a source may return fewer items than asked,
and `next: None` *means* complete rather than being an obligation to know it
early), none for "a thumbnail of an absent item is `NotFound`" (`Ok(None)` is
a normal answer per the trait's own docs), and none for "a directory advertises
no write" (`capabilities()` derives writes to `false` on every branch, so no
implementation can fail it — a tautology wearing a backend's name). Cases that
cannot fail cost the same to run and buy nothing but a longer pass count.

**A failure names a clause, not a call stack.** Cases return `Failure` rather
than panicking, so a run reports every broken clause instead of stopping at
the first, and each one arrives with the requirement's verbatim `.spec/` text
and what the source did in contract terms:

```text
FAILED enumeration.covers-every-child-exactly-once [SYNC-003]
  clause:   Enumeration is paginated and repeatable for a declared snapshot/version;
            duplicate or missing children across pages are contract failures.
  claim:    paging through a listing serves every child once — no duplicate, no hole
  observed: child chat:100/year:2000 was served twice across 3 pages
```

`tests/conformance.rs` holds the suite to that claim from the other side: it
runs it against sources built to break one clause each — a duplicated child, a
drifting snapshot, a cursor served regardless of scope, a delivery past its
range, a delivery of the right offsets with the wrong bytes, a miscategorized
failure — and asserts the suite fails, on the case that owns that clause. A
suite whose every case asserted `true` would pass the fake exactly as loudly;
those tests are what say the cases have teeth.

## The synthetic large account

`synthetic::generate` deterministically expands a `SyntheticSpec` into a
whole account of source facts in the model vocabulary — no SQL, no consumer
knowledge. `SyntheticSpec::large_account()` is the acceptance fixture of
TASK-260715-1ceq7h (2,048 chats, 110,000 messages, ~25k attachments,
Zipf-skewed so a few chats are enormous and the tail is empty), used by
gramdrive-state's EXPLAIN evidence and reused by later performance tasks so
every "fast enough" is measured against the same account.

```rust
use gramdrive_testkit::synthetic::{self, SyntheticSpec};

let account = synthetic::generate(&SyntheticSpec::large_account());
assert!(account.message_total() >= 100_000);
```

Timestamps run on a synthetic calendar of uniform 31-day months from
2024-01-01 (`synthetic::partition_of` gives the `(year, month)` partition as
pure integer arithmetic). Same spec, same account, bit for bit — the unit
tests pin totals and a structural digest, so a distribution change is a
deliberate edit, not drift.

## Test command

```sh
cargo test -p gramdrive-testkit
```
