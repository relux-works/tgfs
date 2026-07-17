# TASK-260715-1ffbkg — Review verdict (round 2): ACCEPTED

Rework re-verified independently, not read. `make check` **8/8**,
`cargo test -p gramdrive-model` **88/88** (18 unit + 9 fixture + 15 collision +
10 property + the identity/tree suites). Both rework findings are closed, and
the fix that shipped is **better than the one the previous review asked for**.

Verdict: `done`. One doc-comment inaccuracy recorded below as a follow-up nit —
not blocking, no behavioural effect.

---

## The previous review's recommendation was wrong. The implementer was right.

Round 1 (me, previous cycle) recommended `to_uppercase().to_lowercase()` and
claimed it "catches every case above", over-colliding "only on `ß`/`ss`, which is
the safe direction". The implementer measured it instead of adopting it and found
a hole that is **not** in the safe direction: `ẞ` (U+1E9E) vs `ß`. APFS case-folds
both to `ss`; the round trip sends `ß`→`ss` but `ẞ`→`ß`, so it **splits** a pair
Apple merges — the same shadowed-folder defect the fix existed to close, one
codepoint over.

I reproduced this against the shipped suite rather than taking their word:

| mutant applied to `fold_key` | shipped suite says |
|---|---|
| `to_lowercase().nfc()` (the round-1 bug) | **FAILS** — unit (`Apple folds "ΟΔΟΣ"/"οδοσ" into one entry`), fixture corpus, sigma goldens |
| `to_uppercase().to_lowercase().nfc()` (**my** round-1 recommendation) | **FAILS** — `case 'capital sharp s vs sharp s': ["ẞ", "ß"] still collide on Apple` |
| shipped implementation | green |

Rejecting a reviewer's named fix requires evidence, and the evidence was produced
and is reproducible. That is the right outcome, and it is worth saying plainly:
the round-1 verdict would have shipped a second `ΟΔΟΣ`/`οδοσ`.

## Finding 1 — closed, verified exhaustively (not by sampling)

`Platform::fold` now models each platform's real rule (Windows → `to_uppercase`
per NTFS `$UpCase`; Apple → full Unicode case folding via `caseless`;
Android/Linux → identity), and `fold_key` **composes** all four, derived from
`Platform::ALL` the way `ComponentBudget::strictest` folds over the same list.

The load-bearing safety claim is *"if any platform merges two names, `fold_key`
merges them too"*. I checked it independently over the **entire char space** —
grouping all 1,112,064 codepoints by each platform's fold and verifying every
group maps to one key (equivalent to the pairwise check without the O(n²)):

```
--- key order: shipped ALL order [Apple, Windows, Android, Linux]
  Apple   merges but key separates: 0 hole(s)
  Windows merges but key separates: 0 hole(s)
  Android merges but key separates: 0 hole(s)
  Linux   merges but key separates: 0 hole(s)
--- key order: reversed [Linux, Android, Windows, Apple]
  ... 0 hole(s) on every platform
```

Probe: `.temp/TASK-260715-1ffbkg/foldprobe/` (standalone crate, path-dep on
`gramdrive-model`; `crates/` untouched). The composition is order-independent, as
claimed.

## Finding 2 — closed; the tests are no longer tautological

- The fold corpus is **pair-shaped** with a hand-written `merged_by` column
  (filesystem truth, not a prediction of the code), asserted against
  `Platform::fold`. The insight that a one-output-per-input table structurally
  cannot catch a wrong fold is correct and generalises.
- Greek sigma (3 pairs), dotless `ı`, `İ`, `ẞ`/`ß`, `ß`/`ss`, ligature, Kelvin,
  Angstrom pinned; sigma and dotless-i suffix goldens present as the rework asked.
- `resolved_siblings_never_collide` and `case_variant_siblings_never_collide` fold
  by `Platform::fold`, platform by platform.

**The claim that re-pointing the broad property was not enough is true**, and I
confirmed the targeted property is what bites. Measured under the `to_lowercase`
mutant with the regressions file removed (pure random search, default 256 cases):
**caught 9/10 runs** — so "fails within a few hundred cases" is accurate, though
random search alone is marginally flaky. It does not matter in practice: the
checked-in `naming_properties.proptest-regressions` seed (`"II"`/`"Iı"`) replays
first and makes the catch **deterministic**, and three other tests (unit, corpus,
goldens) catch the same mutant deterministically regardless.

## Non-blocking: the composition doc names the wrong half as "free"

`naming.rs:707` and `naming.rs:1006` both state that composing gives the
guarantee **"for the last fold applied"** and leaves the earlier ones to Unicode's
tables. That is inverted — it is the **first** applied fold that is free:

With `ALL = [Apple, Windows, Android, Linux]`, `fold_key = Windows(Apple(x))`.
- `Apple(a) == Apple(b)` ⟹ `Windows(Apple(a)) == Windows(Apple(b))` ⟹ keys equal.
  Holds for *any* functions — **Apple, applied first, is the free half**.
- `Windows(a) == Windows(b)` implies nothing about `Windows(Apple(a))` vs
  `Windows(Apple(b))` unless Apple preserves Windows' equivalence classes — a
  Unicode fact, and the one my probe verified above.

The sentence contradicts itself: it says "last is free", then correctly names
`Platform::Windows.fold` — which *is* the last effective fold — as the half that
is *not* free. The actionable half (Windows is the Unicode-dependent guarantee,
so the suite must check it) is stated correctly and the suite does check it, so
nothing is mis-tested. But the abstract framing is backwards in both doc sites and
in the rework write-up, and a maintainer who trusted it could conclude the
Windows check is tautological and drop the only non-free check in the suite.

Comment-only fix (`last` → `first`, `earlier` → `later`). Not worth a rework cycle
against an implementation this thoroughly verified; folded into the next task that
touches `naming.rs`.

## Also verified

- `caseless` 0.2.2, **MIT**, inside the POL-6 allow list; `cargo deny` green; only
  transitive dep is `unicode-normalization`, already carried. The workspace note
  that caseless re-enables its default features transitively is accurate.
- The dep is load-bearing: `ẞ`/`ß` is unreachable without CaseFolding data, and it
  is the only Apple-only pair surviving NFC — the Kelvin/Angstrom singletons are
  settled by NFC before folding, as the logbook says.
- LOGBOOK entry is accurate and matches what I measured.
- Untouched per scope: strip-before-NFC ordering, grapheme truncation, held-back
  escape byte, digest suffixing, loop bound. Confirmed unchanged.
- No reviewer edits to `crates/` — mutation testing ran in a throwaway
  `git worktree` under `.temp/`, since removed.

## Carried forward (unchanged from round 1, still not this task's problem)

`resolve_siblings` needs the whole sibling set while the tree enumerates lazily
per page — a consumer naming page 1 must materialize every sibling. Inherent to
SYNC-012 and correctly deferred to the provider-adapter task; worth an explicit
architecture note there.
