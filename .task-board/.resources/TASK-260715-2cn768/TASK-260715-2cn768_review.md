# TASK-260715-2cn768 — Review verdict: ACCEPTED

Reviewer: [reviewer] reviewer (claude), run RUN-260717-fb7986
Reviewed at commit 8244a6d (working tree, task changes uncommitted).

## Verdict

**Accepted → done.** AC and DoD met. Every claim in the implementer's notes that
I could test independently reproduced. No rework requested.

## AC verification

AC: "Commands are documented, deterministic, and fail on formatting, denied
lints, forbidden licenses, or known critical vulnerabilities according to policy."

The implementer verified clippy/architecture/`--require-clean` fire, but had not
demonstrated the **licenses** and **advisories** gates firing — and the workspace
has **zero third-party dependencies** (7 internal crates, `Cargo.lock` = 7
packages), so `cargo deny check` passes against an empty set and proves nothing
on its own. I tested all four AC failure modes myself in a throwaway copy
(`.temp/TASK-260715-2cn768/deny-probe/`, since removed; main worktree confirmed
byte-identical before and after).

| AC failure mode | Probe | Result |
|---|---|---|
| formatting | injected badly-formatted fn | `format` step FAILED, exit 1 |
| denied lints | injected `println!` + `.unwrap()` | clippy errored: "use of `println!`", "used `unwrap()` on an `Option` value" |
| forbidden licenses | added `unicode-ident 1.0` (`(MIT OR Apache-2.0) AND Unicode-3.0`) | `licenses FAILED` — rejected `Unicode-3.0`, exactly the case deny.toml's comment predicts. `cargo deny` exit 4 |
| known vulnerabilities | added `time =0.1.44` (RUSTSEC-2020-0071) | `advisories FAILED`, exit 1 |

Exit codes propagate correctly through the entrypoint: a failing supply-chain
step returns 1 (EXIT_FAILED), not a false green.

Documented: README "Running the checks" + tool table, crates/README "Commands"
+ suite table, `make gates`, `--list`. Deterministic: exact 1.91.0 pin, and
`check_toolchain.py` asserts the pin is actually *in effect* (rust-toolchain.toml
only binds when rustup drives cargo) rather than assuming it. The one
non-hermetic check (advisories, DB-dependent) is explicitly called out as
intentional in deny.toml and crates/README rather than hidden.

## Gate suite

Re-ran `--suite all` independently: **8/8 passed** (toolchain, format, lint,
test, architecture, supply-chain, traceability, scripts). Provenance at
`.temp/acceptance/review-fb7986/`. Active toolchain rustc/cargo 1.91.0 matches
the pin; cargo-deny 0.20.2 ≥ the 0.18 floor; `cargo deny check` clean with no
deprecation warnings.

## Independent confirmation of the LTO finding

The implementer's headline finding is written into `Cargo.toml` and
`crates/README.md` as a permanent caveat, so I verified it rather than trusting
it — a wrong caveat in a manifest is worse than none.

**Confirmed accurate.** For `gramdrive-ffi` (`crate-type = ["lib", "staticlib",
"cdylib"]`), the release rustc line carries `codegen-units=1` and
`overflow-checks=on` but **no `-C lto`**. Counterfactual: with `crate-type =
["cdylib"]` alone, `lto=thin` appears. The rlib output is the cause, exactly as
documented.

Accepting the disposition (keep + document loudly, route the fix to packaging)
over deleting the setting: it is correct for every crate-type that does link,
and it is not silently dead — both `Cargo.toml` and `crates/README.md` state it
applies to nothing that ships today, with "do not read `lto = "thin"` as a
statement about the binary in a release." Handoff target verified real and
correctly scoped: TASK-260715-3akqs8 "Package shared core for native consumers"
owns the shipped artifact's crate-type.

## Other checks

- **Handoff targets exist and match scope**: 3akqs8 (packaging), 3faqmr (CI /
  blind cross-build), 265gqq (UniFFI) — all present on the board.
- **Spec references resolve**: NFR-050, NFR-052, NFR-012, NFR-030, POL-5, POL-6,
  DEC-018 all found in `.spec/`.
- **Scope correction #2 accepted**: POL-5 does fix v1 at one target, so a probe
  target would gate on an out-of-scope platform and be trivially green against
  stub crates. Correcting the doc and naming the live limitation beats inventing
  a check that proves nothing.
- **Architecture check #11** (`[lints] workspace = true` opt-in) guards a real
  silent failure: a crate omitting the stanza is exempt from the whole lint set
  while the gate stays green. All 7 crates carry the opt-in.
- **Fits the architecture**: config-only, no product Rust source touched. The
  one-entrypoint (barycenter) shape means Makefile and CI are shorthand, never a
  second copy of a gate command.
- **Tests**: 49 script self-tests, behavior-focused, covering the properties that
  matter (steps still run after a failure, `--require-clean` refuses and runs
  nothing, provenance contents, missing tool ≠ runner crash, suite resolution).
- **Hygiene**: `__pycache__/` and `.temp/` are already gitignored; no stray
  artifacts would be committed.

## Non-blocking nits (sweep up on next touch, not worth a rework cycle)

1. `deny.toml:67` — typo "RRUSTSEC ID" → "RUSTSEC ID" (comment only).
2. Board notes and LOGBOOK 0356 say "47 tests"; the suite actually runs **49**.
   Doc drift in the notes, not in the shipped docs.
