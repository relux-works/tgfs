# TASK-260715-3o8wpt — Review verdict: changes requested (to-dev)

Reviewer pass, 2026-07-17. Read-only review; all probes ran against a copy
under `.temp/TASK-260715-3o8wpt/review-probe/`, the real tree was never
modified and re-verified clean afterwards.

## What passes (independently re-verified, not taken from implementer logs)

- `cargo build --workspace` — green; `cargo test --workspace` — 10 unit
  tests + doc-test suites, 0 failures.
- `python3 .scripts/check_crate_architecture.py` — "OK: 7 crates conform".
- `cargo deny check licenses` — "licenses ok"; `deny.toml` allow list is
  exactly the POL-6 set, fail-closed, `all-features = true`, private crates
  skipped. Correct.
- `cargo fmt --all -- --check` — clean; `cargo clippy --workspace
  --all-targets` — 0 warnings.
- `Cargo.lock` contains exactly the 7 workspace crates — zero external deps.
- All 7 per-crate READMEs have `## Ownership` (real board story/task IDs) and
  `## Test command`. `crates/README.md` layer/direction/feature policy is
  coherent and matches every manifest.
- Positive enforcement verified by probe A: injected direction violation
  (render→state), testkit-as-normal-dep, removed README, and simple
  `#[cfg(target_os)]` → 5 errors, exit 1. Script catches what it claims for
  these classes.
- Design decisions (sources as separate crates not features per
  DEC-003/DEC-005; `gramdrive-*` naming per POL-7; testkit dev-only; ffi as
  graph top with rlib+staticlib+cdylib) fit `.spec/architecture.md`. Root
  README and LOGBOOK updated honestly.

## Confirmed defects (evidence: TASK-260715-3o8wpt_review-probe.log)

Both are in this task's own deliverable ("architecture checks" is in the task
scope) and falsify the stated claim in the script docstring ("No platform cfg
predicates ... appear in platform-neutral crate sources") and in
`crates/README.md` ("Everything in this document is enforced by ...").

### D1. cfg scan (check 7) misses 4 common predicate forms

`PLATFORM_CFG_RE` only matches `cfg(` + optional `any(`. Probe B injected all
of the following into `gramdrive-model/src/lib.rs` and the check passed with
exit 0:

- `#[cfg(all(unix, feature = "never"))]` — `all(` not handled
- `#[cfg(not(windows))]` — `not(` not handled
- `#[cfg_attr(windows, allow(dead_code))]` — `cfg_attr` never matches
- `cfg!(target_os = "macos")` — macro form (`cfg!(`) never matches

These are ordinary forms a dev writes without thinking; platform-conditional
behavior would land in a core crate silently.

### D2. Target-gated dependencies are invisible (manifest-level leakage)

Probe C added to `gramdrive-state/Cargo.toml`:

    [target.'cfg(target_os = "macos")'.dependencies]
    gramdrive-model2 = { path = "../gramdrive-model", package = "gramdrive-model" }

Check passes, exit 0. The script ignores the `target` field that
`cargo metadata` reports on each dependency, so any platform-conditional dep
(including non-banned external crates once deps arrive) bypasses the
platform-neutrality rule entirely.

## Required changes

1. Strengthen the cfg detection so at minimum `all(`/`not(` nesting,
   `cfg_attr(...)`, and the `cfg!(...)` macro form are caught. Simplest
   robust shape: on comment-stripped code lines, flag any line matching
   `(?:cfg|cfg_attr)\s*[!(]` that also contains
   `\b(target_os|target_family|target_vendor|windows|unix)\b`. Fail-closed
   false positives (e.g. the words in a string literal) are acceptable for
   this project; document the tradeoff in the script docstring as done for
   block comments.
2. In platform-neutral crates, error on any dependency whose
   `cargo metadata` entry has a non-null `target` (any section, dev
   included) — a target-gated dep is platform leakage regardless of the dep
   name.
3. Extend the negative-check evidence (board log) with these forms so the
   next regression is visible, and keep `crates/README.md` + docstring claims
   in sync with what is actually enforced.

## Recommended, not blocking

- Consider scanning core-crate sources for `std::os::` paths
  (`std::os::unix::...` compiles per-platform with no cfg and no dep) —
  cheap to add; alternatively note it as a known limitation until CI
  cross-builds exist (TASK-260715-2cn768 territory).

## Verdict

**to-dev.** The workspace, layering, license gate, docs, and green gates are
accepted as designed — no architectural rework wanted. Rework is confined to
`.scripts/check_crate_architecture.py` (+ negative-check log refresh and
doc-claim sync). AC element "no platform leakage ... enforced by a check" is
not yet true for the forms above.
