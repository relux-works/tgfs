# TASK-260715-1ffbkg — Rework results: platform-true case folding

Rework per `TASK-260715-1ffbkg_review-verdict.md`. Both findings addressed.
`make check` **8/8**. Everything the review verified correct (strip-before-NFC,
grapheme truncation, held-back escape byte, digest suffixing) untouched.

**The review's recommended fix was insufficient. It is not what shipped.**
Details below — this is the one thing worth reading if you read nothing else.

---

## The recommended round trip has a hole: `ẞ`

The verdict recommended `to_uppercase().to_lowercase()`, noting it "catches
every case above" and "over-collides only on `ß`/`ss`, which is the safe
direction". Measured over the whole `char` space against `caseless` as ground
truth (`.temp/TASK-260715-1ffbkg/coarseprobe/`), it has **exactly one hole**,
and it is not a safe-direction one:

| pair | APFS (case fold) | round trip | verdict |
|---|---|---|---|
| `ẞ` (U+1E9E) vs `ß` | merges → `ss` | **splits** (`ß` vs `ss`) | **missed collision** |
| `ẞ` vs `ss` | merges → `ss` | **splits** | **missed collision** |

`ß` uppercases to `SS` → lowercases to `ss`; `ẞ` uppercases to itself → lowercases
to `ß`. So the round trip separates two names APFS resolves to one directory —
the *same class of defect* the fix was written to close, one codepoint over.
Adopting it as specified would have shipped a second `ΟΔΟΣ`/`οδοσ`.

## No single mapping models the platforms — in either direction

| pair | `to_lowercase` | `to_uppercase` | round trip | true case fold | **composed** |
|---|---|---|---|---|---|
| `ΟΔΟΣ`/`οδοσ` | miss | catch | catch | catch | catch |
| `οδος`/`οδοσ` | miss | catch | catch | catch | catch |
| `ı`/`i` (NTFS merges) | miss | catch | catch | **miss** | catch |
| Kelvin `K`/`K` (APFS merges) | catch | miss | catch | catch | catch |
| `ẞ`/`ß` (APFS merges) | miss | miss | **miss** | catch | catch |

Note row 3: **true Unicode case folding also fails** — `caseless` alone was the
verdict's other suggestion, and it misses `ı`/`i`, which NTFS `$UpCase` merges.
Neither case-insensitive platform's fold contains the other. There is no single
mapping to pick; the key has to compose them.

## What shipped

**Finding 1 — `Platform::fold` (`naming.rs`).** Each platform's real rule, as
filesystem truth alongside `Platform::check`:

- **Windows** → `to_uppercase` (NTFS `$UpCase` folds via *uppercase*). Documented
  fidelity gap: Rust's full mappings send `ß`→`SS` where `$UpCase` leaves it, so
  the model is *stricter* than NTFS — costs a suffix, never a shadowed folder.
- **Apple** → full Unicode case folding via `caseless`.
- **Android/Linux** → identity (ext4/f2fs compare bytes).

`fold_key` composes all four, **derived from `Platform::ALL`** the way
`ComponentBudget::strictest` folds over the same list — a platform added later
tightens the key instead of leaving a constant to drift. Verified 0 holes
against both platform folds across the whole char space, in either composition
order, and idempotent.

**Finding 2 — tests fold by the platform, not by the implementation.** The
property suite and the corpus now assert against `Platform::fold`. Composing
gives distinctness under the last fold applied *for free* and says nothing about
the earlier ones — so the test is not tautological, and the module docs say
which half is assumed and which is checked.

**Corpus (`naming_fixture.rs`)** — a fold corpus of **pairs** with a
hand-written `merged_by` column (which platforms merge each pair). The general
constraint worth carrying forward: *a one-expected-output-per-input table
structurally cannot catch a wrong fold* — every row passes while two rows
silently name one folder. Folding is a property of pairs and needs a
pair-shaped table. Greek sigma (3 pairs), dotless `ı`, `ẞ`/`ß`, `ß`/`ss`,
ligature, Kelvin, Angstrom pinned, plus suffix goldens for the sigma and
dotless-i pairs as the rework asked.

## Measured, not assumed: re-pointing the property suite was not enough

Re-pointing the *broad* property at `Platform::fold` fixes the tautology but
**does not catch the bug**: `arb_raw` samples too wide a space to land two
siblings on one folded name. Measured — with the `to_lowercase` key restored,
`resolved_siblings_never_collide` stays **green**. Reporting the re-point as the
fix would have been false comfort.

So the suite gained a targeted `case_variant_siblings_never_collide` over an
alphabet of nothing but characters the platforms fold differently, which fails
within a few hundred cases. `CASE_VARIANTS` chars also added to `NASTY` so a
fold can compound with a device name or a truncation. The doc comments state
which property actually bites and which does not.

## Mutation-verified

Both defects reintroduced against the final suite:

| mutant | result |
|---|---|
| `to_lowercase().nfc()` (the shipped bug) | **FAILS** — unit, fold corpus, goldens, targeted property |
| `to_uppercase().to_lowercase().nfc()` (the review's recommendation) | **FAILS** — `case 'capital sharp s vs sharp s': ["ẞ", "ß"] still collide on Apple` |
| final implementation | green, `make check` 8/8 |

A green suite proves nothing unless it fails on the bug, so this was run rather
than argued. `fold_key_merges_whatever_any_platform_merges` (unit test) pins the
load-bearing assumption directly and includes the `assert_ne!`s showing why each
stock mapping was rejected — "just lowercase it" is the obvious simplification,
and it shipped a bug.

## Dependency

`caseless` 0.2.2, **MIT** — inside the POL-6 allow list, no DEC-021 exception
needed; `cargo deny` green. Only dep is `unicode-normalization`, already carried.
Load-bearing: `ẞ`/`ß` is unreachable without CaseFolding data. Noted in the
workspace manifest that caseless re-enables `unicode-normalization`'s default
features transitively, so the `default-features = false` above it is not a
guarantee — harmless (the model crate is std) but not left to mislead.

## Findings worth carrying forward

- **NFC settles the compatibility singletons before folding is reached.** Kelvin
  U+212A → `K` and Angstrom U+212B → `Å` are NFC singletons, so those pairs are
  one name even on case-sensitive Linux and never were fold cases. The only
  Apple-only pair that survives NFC — and the entire justification for the
  `caseless` dep — is `ẞ`/`ß`.
- The `Platform::case_insensitive()` bool is now documented as "whether `fold`
  is anything but identity", tying the claim to the behaviour behind it.

## Not touched

Per rework scope: strip-before-NFC ordering, grapheme truncation, held-back
escape byte, digest suffixing, loop bound. The review's non-blocking note on
lazy paging vs. set-relative naming remains an architecture note for the
provider-adapter task; unchanged here.

## Verification

- `make check` — **8/8** (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts). Log: `.temp/TASK-260715-1ffbkg/make-check-04.log`
- `cargo test -p gramdrive-model` — naming: 18 unit + 9 fixture + 15 collision +
  10 property, all green.
- Probe: `.temp/TASK-260715-1ffbkg/coarseprobe/` (standalone crate; `crates/` untouched).
