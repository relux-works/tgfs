# TASK-260715-1ffbkg — Review verdict: CHANGES REQUESTED

Reviewer re-ran everything independently. `make check` **8/8**, `cargo test -p gramdrive-model`
**83/83**. Implementation quality is high and the documentation is genuinely
excellent. One **confirmed correctness defect** blocks acceptance.

---

## FINDING 1 (blocking) — case-fold key misses collisions the target platforms make

`naming.rs:630` — `fold_key` folds with `str::to_lowercase`:

```rust
fn fold_key(name: &str) -> String {
    name.to_lowercase().nfc().collect()
}
```

The doc comment on `resolve_siblings` claims this is *"at least as aggressive as
the platforms' own tables"*. **That claim is false.** `to_lowercase` applies the
Unicode **contextual final-sigma rule**, emitting `ς` at word end, while NTFS's
`$UpCase` table folds via *uppercase* (`ς`→`Σ`, `σ`→`Σ`) and APFS folds via true
Unicode *case folding* (`ς`→`σ`). Both collapse what `to_lowercase` keeps apart.

### Reproduced through the public API (not a thought experiment)

```
Greek CAPS vs lowercase    -> ["ΟΔΟΣ", "οδοσ"]  suffixed=false  windows_same_dir=true  *** BROKEN ***
final vs medial sigma      -> ["οδος", "οδοσ"]  suffixed=false  windows_same_dir=true  *** BROKEN ***
Turkish dotless i vs i     -> ["ı", "i"]        suffixed=false  windows_same_dir=true  *** BROKEN ***
Latin Bob vs BOB           -> ["Bob (47fjxm4)", "BOB (27ngzyb)"]                       ok
```

Two sibling chats titled `ΟΔΟΣ` and `οδοσ` — an ordinary Greek word in caps and
in lowercase — receive **no collision suffix**. On Windows (and case-insensitive
APFS) they resolve to **one directory**: one chat's folder silently shadows the
other's. That is exactly the failure SYNC-012/SYNC-013 and the DoD item
*"case-insensitive collision handling"* exist to prevent.

Probe: `.temp/TASK-260715-1ffbkg/sigmaprobe/` (standalone crate, path-deps on
`gramdrive-model`; `crates/` untouched).

### Root cause is structural, not a typo

`Platform` models per-platform *character* rules, budgets, and exposes
`case_insensitive() -> bool` — but it never models **how** each platform folds.
So the fixture corpus, which asserts "one output satisfies all four platforms",
structurally cannot catch a folding mismatch. The bool is a claim about case
sensitivity with no fold behind it.

### Recommended fix

Neither single-direction mapping is sufficient — each misses what the other catches:

| pair | `to_lowercase` | `to_uppercase` | `to_uppercase().to_lowercase()` |
|---|---|---|---|
| `ΟΔΟΣ` vs `οδοσ`   | miss | catch | catch |
| `οδος` vs `οδοσ`   | miss | catch | catch |
| `ı` vs `i`         | miss | catch | catch |
| `ß` vs `ss`        | miss | catch | catch |
| Kelvin `K` vs `K`  | catch | **miss** | catch |
| Angstrom `Å` vs `Å`| catch | **miss** | catch |

The round-trip `to_uppercase().to_lowercase()` catches every case above with no
new dependency. It over-collides on `ß`/`ss` — which is the **safe** direction,
by the implementer's own stated principle: *"a spurious collision costs a
suffix; a missed one would ship a broken tree."* A true Unicode case fold
(e.g. `caseless`) is the more principled option if a dep is acceptable.

Keep the existing `.nfc()` re-normalization either way.

## FINDING 2 (blocking, same root) — the property test mirrors the bug, so it cannot fail

`naming_properties.rs:169` asserts collision-freedom by folding with
`to_lowercase()` — **the same function the implementation uses**:

```rust
let folded: HashSet<String> = names.iter().map(|name| name.to_lowercase()).collect();
prop_assert_eq!(folded.len(), names.len());
```

This is tautological for this bug class: `resolve_siblings` iterates until
`fold_key` reports no collisions, so a test folding the same way is guaranteed to
pass regardless of whether the fold matches any real filesystem. It restates the
implementation instead of checking it.

The test must fold by the **platform's** rule, not the implementation's — ideally
via a new `Platform::fold(&str)` that the corpus and the property suite both
assert against, which would also close the structural gap in Finding 1.

---

## Non-blocking observations (no action required for acceptance)

- **`resolve_siblings` loop bound is adequate.** `min(len + 2, 64)` looked tight
  (worst-case useful bumps are `3 × len`), but the bound holds: escalation chains
  run in *parallel* rounds, not sequentially, and each additional round requires a
  distinct sibling crafted to impersonate another's suffixed name — so rounds are
  bounded by the longest chain (≤ `len`), not the sum. Verified by hand-tracing a
  4-link chain. The code's "backstop" framing is accurate.
- **Lazy paging vs. set-relative naming.** `resolve_siblings` needs the *whole*
  sibling set, while the tree enumerates lazily per page. A consumer naming page 1
  must materialize every sibling, since a collision may involve page 50. This is
  inherent to SYNC-012 (correctly flagged by the implementer as a provider-layer
  concern), but it is worth an explicit architecture note before the adapter task —
  it partly defeats the paging design from TASK-260715-3tjduq.
- **Verified claims:** 53-case corpus ✓; 37 new tests (7 unit + 6 fixture + 15
  collision + 9 property) ✓; new deps MIT OR Apache-2.0 inside the POL-6 allow
  list, no DEC-021 exception needed, `cargo deny` green ✓; traceability maps
  SYNC-012/013 + PLAT-021 to this task ✓; `tree.rs` refactor is behaviour-preserving ✓.
- **Genuinely good work:** the `is_nfc_quick`→`is_nfc` bug found and pinned by the
  property suite, the strip-before-NFC ordering, grapheme-boundary truncation, the
  held-back escape byte, identity-digest-over-prefix reasoning, and suffixing every
  member of a collision set are all correct and well-argued. Everything except the
  fold is right.

## Verdict

→ `to-dev`. Findings 1 and 2 are ordinary implementation rework: fix `fold_key`,
make the property/corpus assert against a platform-modeled fold rather than the
implementation's own, and add the Greek-sigma and dotless-i pairs to the corpus so
the regression is pinned. Not a stop-the-line blocker — no external decision needed.
