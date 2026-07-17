# TASK-260715-3e8q4m — DriveSource conformance suite

Backend-agnostic conformance suite in `gramdrive-testkit::conformance`. 38 cases
over 13 contract clauses, running unchanged against any `DriveSource` via a
generic harness entry. Status: ready for review.

## Entry points

```rust
use gramdrive_testkit::conformance::{self, FakeHarness};

let report = conformance::run(&FakeHarness::new());   // never panics, reports everything
conformance::assert_conforms(&harness);               // #[test] entry; fails with the whole report
```

`run<H: SourceHarness>(&H) -> Report` is generic over the harness, not a trait
object — `block_on` is generic in the future's output because how futures are
driven is the backend's business (the fake needs no runtime; tdjson needs tokio).

## Design

| Piece | Role |
|---|---|
| `SourceHarness` | The seam: `name`, `supports`, `block_on`, `stage`. One impl per backend |
| `WORLD` / `WorldSpec` | The one fixed world every case is staged against |
| `Landmarks` | Where the harness put each part of it — identities only |
| `Perturbation` / `Setup::arm` | Armed *before* the source goes live: unreachable, expired reference, rate limit, auth revoked, slow, mid-fetch race |
| `Mutation` / `Setup::plan` / `Control` | Applied *while* live: child appears/removed, content changes |
| `Capability` | What a harness can stage; what it declines is **skipped**, never passed |
| `Report` / `Clause` / `CaseOutcome` | Per case: clause pinned, claim made, what the source did instead |

Arming and mutating are split because backends implement them differently. The
mutation plan is declared up front so a harness can prepare — the fake compiles
it into change batches at build time (`SourceScript` is immutable; `advance()`
walks pre-validated revisions).

The suite lives in `src/`, so the workspace's `unwrap`/`panic` denials apply:
cases return `Failure` rather than asserting. That is also what makes the
report possible — a run reports every broken clause instead of stopping at the
first. `assert_conforms` is the only panic, and its caller is a test.

## Coverage — 38 cases / 13 clauses

| Clause | Cases |
|---|---|
| SYNC-001 | root is a parentless directory; children name their parent; a fetchable file is served as readable content |
| SYNC-003 | every child once (no dup/hole); one snapshot per enumeration; repeatable; order independent of page size; page size respected; empty directory; file → InvalidRequest; absent → NotFound; no splice when the listing moves |
| SYNC-004 | cursor carries the source's scope; survives a durable round trip; another account's rejected; another namespace epoch's rejected |
| SYNC-022 | drained feed reports nothing; reports an unseen change; an applied page advances past its changes |
| SYNC-025 | a removal arrives as an explicit event |
| SYNC-041 | full range; partial range; suffix range starts at its own offset; range past extent → InvalidRequest; directory → InvalidRequest; absent → NotFound |
| SYNC-042 | stale pin conflicts before any byte moves; losing a race never completes (delivered bytes are a prefix of the *pinned* version) |
| SYNC-043 | sink Stop → Cancelled; abandoned fetch re-fetches whole |
| SYNC-044 | unreachable recovers on retry; expired reference is refreshable; rate limit carries its backoff; lost auth reported as such |
| SYNC-045 | a reference refresh does not move identity |
| SYNC-046 | concurrent fetches do not corrupt each other |
| POL-4 | restricted refused through content *and* thumbnail |

AC mapping: pagination ✓ · cursor durability ✓ (encode/decode is exactly what a
restart does) · version races ✓ · range correctness ✓ · retries ✓ · cancellation
✓ (both paths) · capabilities ✓ · account/schema mismatch ✓ (both, derived from
the source's own scope, not a fixture constant).

## Backend-independence

The suite never constructs a source, reaches past the trait, or learns a
page-token format. Mismatch cases derive a foreign scope from `source.scope()`
rather than a fixture constant. No case asserts the harness's own child order —
they assert the source agrees with *itself* across enumerations and page sizes.
`does_not_splice_when_the_listing_moves` accepts both legal answers (reject the
continuation, or keep serving the snapshot) and fails only the splice.

Clause `statement()` is verbatim `.spec/` text, not a paraphrase — a summary is
where a suite quietly acquires opinions the spec does not hold.

## Proof the cases have teeth

`tests/conformance.rs` runs the suite against `Saboteur` sources that break one
clause each, asserting the suite fails on the case owning that clause:

| Sabotage | Caught by | Clause |
|---|---|---|
| duplicates a child | enumeration.covers-every-child-exactly-once | SYNC-003 |
| shifts the snapshot per page | enumeration.is-one-snapshot | SYNC-003 |
| serves any cursor | cursor.another-accounts / another-namespace-epochs | SYNC-004 |
| overruns the range | fetch.a-full-range-delivers-exactly-the-content | SYNC-041 |
| right offsets, wrong bytes | fetch.a-full-range… and fetch.concurrent-fetches… | SYNC-041 / SYNC-046 |
| miscategorizes failures | failure.lost-authorization… and failure.an-expired-reference… | SYNC-044 |

Plus: an "austere" harness supporting nothing gets skips, stays conformant, and
is **not** credited with the clauses it never ran (`clauses_upheld`).

## Deliberate non-cases (would false-fail a correct backend)

Found by adversarial review; each was written, then removed once checked against
the contract:

- **"a generous page completes in one page"** — `PageRequest` says a source may
  return fewer items than asked, and `next: None` *means* complete rather than
  obliging a source to know it without another round trip.
- **"an empty directory takes exactly one page"** — same trailing-token family.
- **"a thumbnail of an absent item is NotFound"** — the trait blesses `Ok(None)`
  as "a normal answer, not an error"; only restricted content must fail.
- **"a directory advertises no write" (SYNC-060)** — `capabilities()` hardcodes
  writes to `false` on every branch, so no impl can fail it. Vacuous. SYNC-060
  is also a *native provider* obligation, not this contract's.
- **`!is_complete()` on an abandoned fetch** — a source may deliver a small
  range in one chunk and suspend before resolving.
- **`retry_advice()` on an already-matched variant** — derived exhaustively in
  `gramdrive-source`; asserting it tests that crate, not the backend. The one
  kept payload assertion is `RateLimited::retry_after`, which the backend must
  actually carry across the boundary. Flood-wait backoff is whole seconds, since
  Telegram's `FLOOD_WAIT_n` is integer seconds.

## Files

- `src/conformance/{mod,harness,report,support,fake}.rs`
- `src/conformance/cases/{mod,shape,enumeration,cursors,fetch,failures,cancellation}.rs`
- `tests/conformance.rs` — the fake passes + the saboteurs
- README: `crates/gramdrive-testkit/README.md` § The conformance suite

~4,600 lines including tests and docs.

## Verification

`make check` → 8/8 (toolchain, format, lint, test, architecture, supply-chain,
traceability, scripts). 192 tests pass; conformance report shows
`38 passed, 0 failed, 0 skipped` against the fake.

## Follow-ups (not blocking)

- `Operation` is reused from `crate::fault` for `Perturbation`; a harness author
  imports `gramdrive_testkit::fault::Operation`. Consider re-exporting it from
  `conformance` so the harness API is self-contained.
- `feed.an-applied-page-advances-past-its-changes` forbids replay. Defensible
  from `ChangePage::next` ("the durable position after applying this page"), but
  an at-least-once feed with coarse checkpoints (Telegram `pts`) may legitimately
  re-deliver a batch. If a real backend fails it, that is a contract conversation
  worth having, which is what the failure should trigger.
- `feed.a-drained-feed-reports-nothing` is inherently racy against a live
  account (a change can land between `latest_cursor()` and `changes()`).
