# TASK-260715-3o8wpt — rework results (review verdict D1/D2)

Rework pass, 2026-07-17. Scope per `TASK-260715-3o8wpt_rework-scope.md`:
architecture check script + evidence/docs only. **No `crates/` product code
was touched** — the workspace, layering, manifests, and per-crate READMEs the
review accepted are byte-identical.

Files changed:

- `.scripts/check_crate_architecture.py` — D1, D2, optional `std::os::` scan,
  docstring sync.
- `crates/README.md` — enforced-by claim sync (3 edits).

## D1 — cfg predicate detection broadened

The old `PLATFORM_CFG_RE` matched `cfg(` + optional `any(` only. Replaced with
a two-part, fail-closed scan over comment-stripped source:

1. `CFG_INVOCATION_RE` = `\bcfg(?:_attr)?\s*[!(]` finds every `cfg(...)`,
   `cfg!(...)`, `cfg_attr(...)` invocation.
2. Its **balanced-parenthesis argument span** is then searched for
   `\b(target_os|target_family|target_vendor|windows|unix)\b`.

Scanning the paren span rather than the line is a deliberate step past the
reviewer's suggested line match (which the verdict framed as "at minimum" /
"simplest robust shape"). It is strictly stronger — it also catches predicates
wrapped across lines by rustfmt (case D1e, which a line match misses) — and it
has *fewer* false positives, since the predicate word must sit inside the cfg
arguments rather than anywhere on the line. Cost is ~15 lines.

Fail-closed tradeoffs, now documented in the script docstring and
`crates/README.md`:

- only `//` line comments are stripped; predicate words in block comments or
  string literals are flagged (a false positive costs a rename; a miss costs
  the guarantee);
- stripping treats `//` inside a string literal as a comment start, so a
  predicate following a literal `//` on the same line is missed;
- a cfg invocation with unbalanced parens is scanned to EOF rather than
  skipped.

## D2 — target-gated dependencies

`cargo metadata` reports a non-null `target` on any dep declared under
`[target.'cfg(...)'.dependencies]`. The script now carries that field through
`all_direct` and errors on any non-null `target` in a platform-neutral crate,
in **any** section including dev — a platform-conditional dep is leakage
regardless of the dep name, so this catches non-banned external crates too.

## Optional (taken) — `std::os::` scan

Added as check 9. `std::os::unix::...` compiles per-platform with no cfg and
no dependency, so neither D1 nor D2 would see it. Cheap, and no current source
contains one. The remaining gap — genuine cross-target builds — stays with
TASK-260715-2cn768 and is now named as a limitation in `crates/README.md`
rather than implied away.

## Doc-claim sync

`crates/README.md` claimed "Everything in this document is enforced by
`check_crate_architecture.py`". That was false in both directions, so it now:

- scopes the claim to crate set / dependency direction / platform neutrality,
  and names `cargo deny check licenses` as the license gate's enforcer;
- explicitly lists what is **convention, not enforced**: sources-as-separate-
  crates (DEC-003/DEC-005), the no-cargo-features baseline, layer numbering
  beyond the direction allow list, and cross-target verification;
- spells out the four concrete platform-neutrality rules and the fail-closed
  scan tradeoff.

## Verification

Negative evidence: `TASK-260715-3o8wpt_negative-checks.log`, reproducible via
`TASK-260715-3o8wpt_negative-check-harness.py`. 12 injected-violation cases
(each → exit 1) + 2 controls (each → exit 0). The 8 forms the reviewer proved
were missed — D1a `all(unix,..)`, D1b `not(windows)`, D1c `cfg_attr(windows,..)`,
D1d `cfg!(target_os=..)`, D1e multi-line, D1f simple (NEG-2 regression guard),
D2 target-gated dep, `std::os::` — now all fail closed. Controls confirm
`cfg(test)`, `cfg(feature = ..)` and doc comments naming predicates in prose
stay clean, i.e. the fail-closed scan does not fire on the real tree's own
`//!` docs.

NEG-4 (license gate) is carried over verbatim in the log and was **not**
re-run: reproducing it needs a GPL/MPL crate pulled into the tree, and this
rework touched neither `deny.toml` nor the gate. `cargo deny check licenses`
was re-run on the real tree and is green.

All gates re-run on the real tree after the changes
(`.temp/TASK-260715-3o8wpt/gates-*.log`):

| Gate | Result |
|---|---|
| `cargo build --workspace` | green |
| `cargo test --workspace` | 10 unit tests + doc-tests, 0 failures |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets` | 0 warnings |
| `cargo deny check licenses` | `licenses ok` |
| `python3 .scripts/check_crate_architecture.py` | `OK: 7 crates conform` |

Real tree verified free of probe leftovers (`grep -rn 'probe_' crates/` → no
matches); all injections ran against a scratch copy under
`.temp/TASK-260715-3o8wpt/`.
