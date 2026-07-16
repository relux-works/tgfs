# TASK-260715-3o8wpt — Review verdict #2 (rework): ACCEPTED → done

Reviewer pass, 2026-07-17. Read-only review of the D1/D2 rework per
`TASK-260715-3o8wpt_rework-scope.md`. All probes ran against scratch copies
under `.temp/`; the real tree was never modified.

## Scope compliance

Rework touched exactly what the verdict allowed:
`.scripts/check_crate_architecture.py` and `crates/README.md` (mtimes
03:32/03:33 vs. 03:19–03:21 for every `crates/` product file). No probe
leftovers in the real tree (`grep -rn 'probe_' crates/` clean).

## Gates — independently re-run on the real tree (not taken from logs)

| Gate | Result |
|---|---|
| `cargo build --workspace` | exit 0 |
| `cargo test --workspace` | 14 suites, 10 passed, 0 failed |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets` | 0 warnings |
| `cargo deny check licenses` | licenses ok |
| `python3 .scripts/check_crate_architecture.py` | OK: 7 crates conform |
| `make check` (arch + licenses + traceability + build + test) | exit 0 |

Logs: `.temp/TASK-260715-3o8wpt/review2/gates-*.log`.

## D1 — cfg predicate detection: VERIFIED

Implementation is a balanced-paren argument-span scan over comment-stripped
source (`CFG_INVOCATION_RE` = `\bcfg(?:_attr)?\s*[!(]`, then
`PLATFORM_PREDICATE_RE` inside the span). This is strictly stronger than the
verdict's suggested "at minimum" line match: it also catches predicates
wrapped across lines and has fewer false positives. Accepted as an
improvement, not a deviation.

## D2 — target-gated deps: VERIFIED

Non-null `cargo metadata` dep `target` errors in platform-neutral crates,
any section including dev, name-independent.

## Evidence verification

1. **Board harness reproduces 1:1.** Re-ran
   `TASK-260715-3o8wpt_negative-check-harness.py` verbatim from `.temp/`:
   10 injection cases → exit 1, 2 controls → exit 0, matching
   `TASK-260715-3o8wpt_negative-checks.log` exactly. (Note: the harness
   resolves the repo root as `parents[2]` of its own path, so it must run
   from a copy exactly two levels below the repo root, e.g.
   `.temp/<dir>/probe.py` — board-resource copies fail from their stored
   location. Cosmetic; documented here.)
2. **10/10 independent adversarial probes behaved as claimed**
   (`TASK-260715-3o8wpt_review2-probes.log`), covering forms NOT in the
   implementer's harness:
   - R1 `cfg(any(unix,..))` regression of the original form → caught
   - R2 bare `cfg!(windows)` → caught
   - R3 target-gated **dev**-dependency → caught
   - R4 target-gated **build**-dependency → caught
   - R5 plain-triple gate `[target.x86_64-pc-windows-msvc.dependencies]`
     (no cfg syntax at all) → caught — stronger than the verdict asked
   - R6 banned dep (`winapi`) in dev section → caught
   - R7 renamed target-gated dep (original probe C form,
     `package = "..."`) → caught
   - R8 multi-line `cfg_attr`, predicate on continuation line → caught
   - R9 `target_arch`/`target_env` predicates → pass (documented scope is
     target_os/target_family/target_vendor/windows/unix — matches the
     verdict's required set; see observations)
   - R10 predicate word in a plain string with no cfg → clean (scan is
     span-scoped, not a naive word grep)

## Doc-claim sync: VERIFIED

- Script docstring lists checks 1–10 and the three fail-closed scan
  tradeoffs; all match the implementation.
- `crates/README.md` now scopes the enforced-by claim to crate set /
  direction / platform neutrality, names `cargo deny` as the license
  enforcer, lists convention-not-enforced rules explicitly, and names the
  cross-target gap as TASK-260715-2cn768's.
- LOGBOOK 0352 records the rework.

## Observations (non-blocking, no action required)

- `target_arch`/`target_env` predicates (e.g. `cfg(target_env = "msvc")`)
  are outside the scanned predicate set. The set matches the review
  verdict's required list and the documented claim, so this is consistent —
  worth adding words to `PLATFORM_PREDICATE_RE` only if arch/env-gating
  ever becomes a real leakage vector.
- `TASK-260715-3o8wpt_rework-results.md` says "12 injected-violation
  cases"; the regenerated log has 10 fresh cases (NEG-1 contains 2
  injections) + NEG-4 carried over = 12 injected violations across 11
  cases. The log itself is unambiguous; phrasing only.

## Verdict

**done.** The AC element that failed review #1 — "no platform leakage …
enforced by a check" — now holds for every form the first review proved
bypassed, plus stronger forms probed here. Implementation matches AC,
solution fits the architecture, all gates green.
