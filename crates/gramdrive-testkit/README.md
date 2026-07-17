# gramdrive-testkit

Test support shared across the core: the deterministic fake `DriveSource`,
the source conformance suite, and shared fixture trees including
cross-platform filename fixtures (PLAT-021). Product crates may use it only
as a `dev-dependency` — it never ships in a product artifact.

## Ownership

STORY-260715-255sa3 (drive-source-contract), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-3uft8j (deterministic fake
source), TASK-260715-3e8q4m (conformance suite).

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

## Test command

```sh
cargo test -p gramdrive-testkit
```
