# TASK-260715-265gqq — Round-2 rework results (2026-07-17)

Scope: the three stale doc passages named in `TASK-260715-265gqq_review-verdict.md`.
Nothing else touched — no code, no config, no test changes.

## The defect

Three passages still described the pre-DEC-021 world (supply-chain gate red,
licensing decision pending). DEC-021 was owner-accepted 2026-07-17 and the gate
is green, so committing them would have landed a factually wrong compliance
status in the same commit that implements the accepted decision.

## The three fixes

1. **`Cargo.toml`** (comment above the `uniffi` dep) — was "outside the POL-6
   allow list; the supply-chain gate fails until the owner accepts the pending
   licensing decision row". Now: outside the allow list *and* an owner-accepted
   named exception (DEC-021), granted per-crate via `[licenses.exceptions]`, so
   the grant reaches only the named `uniffi*` crates and a new one fails the
   gate until added on purpose.
2. **`crates/README.md`** — the "Known gap — supply-chain gate is red pending a
   licensing decision" paragraph became "Licensing — two named POL-6 exceptions,
   gate green": both MPL-2.0 (uniffi family) and Unicode-3.0 (`unicode-ident`)
   are owner-accepted per DEC-021, enforced as per-crate exceptions rather than
   blanket `allow`, and all four `cargo deny` checks pass. Dropped the stale
   pointer to `docs/OPEN_QUESTIONS.md` as the home of a *pending* decision —
   that file already records the question as resolved by DEC-021.
3. **`crates/gramdrive-ffi/README.md`** ("License gate status") — was "stays red
   until the owner accepts the pending decision row". Now: named exception per
   DEC-021 with the `.spec/decisions.md` pointer, granted to the named `uniffi*`
   crates via `[licenses.exceptions]`; gate green.

Wording follows the already-corrected POL-6 prose in `.spec/policies.md:59`, so
all four descriptions of the exception now agree, and each states the *named,
per-crate* nature of the grant rather than implying a blanket allow.

## Verification

- `make check` → **8/8 green, exit 0** (toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts). Log:
  `TASK-260715-265gqq_gates-round2.log`. Exit code confirmed explicitly — the
  first run's `$PIPESTATUS` read empty under zsh (which uses `$pipestatus`), so
  the suite was re-run without a pipe rather than trusting the summary line.
- Smoke not re-run: doc-only change, per the verdict's explicit instruction.
- Repo-wide grep for `stays red|is red|gate fails|pending decision|pending
  licensing|until the owner accepts` over `*.md`/`*.toml`: no hits in source or
  docs. Remaining hits are only the board's own verdict/progress artifacts and
  LOGBOOK, which are append-only history describing the past state correctly.

## Notes for the reviewer

`git diff` against HEAD shows more than three hunks — the rest is round-1's
still-uncommitted work, unchanged by this round. The round-2 edits are exactly
the three passages above.
