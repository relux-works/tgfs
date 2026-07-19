# TASK-260715-3oe2nr — Build macOS native acceptance suite — results

**Status handed to review.** Ready for review, not accepted/done. The final
Finder run and sign-off is a HUMAN step (checklist item 3 — do not self-close).

## What was built

A human-in-the-loop macOS **native acceptance harness** for the ten File
Provider Finder flows the release gate requires (`.spec/quality-and-release.md`,
macOS spike gates; SYNC-* / PLAT-MAC-*):

- **`.scripts/acceptance/run_native_macos.py`** — one **scenario catalog** is
  the single source of truth for the run-sheet, the evidence form, and the
  machine probes (they cannot drift). It preflights the host against the v1
  matrix (macOS 14+ arm64, POL-5/DEC-017), runs machine-checkable probes where
  robust and captures evidence otherwise (`fileproviderctl dump`, the
  `com.reluxworks.gramdrive` unified log, `codesign`, `spctl`, `stat`), and
  emits `runsheet.md` + `evidence-template.md` + `summary.json` + per-probe logs
  into `.temp/acceptance/<run-id>/` (same provenance root as the gate runner;
  NFR-052).
- **`.scripts/tests/test_run_native_macos.py`** — 30 hermetic self-tests
  (injected command runner + filesystem oracle) run by the `repo` gate suite.
- **`.scripts/acceptance/README.md`** — pipeline + rationale.
- **Makefile**: `accept-macos`, `accept-macos-runsheet`.
- **README.md**: tools-table row.

## The ten scenarios (run order)

registration · enumeration · hydrate · cancel · pin · update · restart · repair
· upgrade · remove — each mapped to its spec requirements and the release-gate
line it proves. `python3 .scripts/acceptance/run_native_macos.py --list`.

## The honesty stance (why human-in-the-loop, not fully automated)

The Finder flows **cannot pass unattended**, and faking them would make the gate
lie:

- they need a **real signed, installed `GramDrive.app`**, a **Telegram test
  account**, and a **person watching Finder**;
- **TDLib is not yet linked into `gramdrive-agent`** (`.scripts/apple-app/README.md`),
  so even "open a dataless file → correct bytes" has no unattended path yet.

So the harness automates only what a machine can *truthfully* check and prepares
the rest. **It never reports a scenario as passed** — a completed run is
`prepared`, awaiting the human sign-off the release gate accepts. This was a
deliberate design choice to avoid a forced fit (a green harness that proves
nothing). `summary.json` states `machine_verdict` per scenario and
`human_verdict: pending`; the overall `result` is never `passed`.

## Verification performed

- `make check-repo` → **traceability ok, scripts ok** (2/2). The new self-test
  is auto-included via the `scripts` gate step.
- `python3 -m unittest … test_run_native_macos` → **30 tests OK**.
- **Live run on this host** (macOS 26.5 / arm64): harness prepared all 10
  scenarios, wrote the run-sheet, evidence form, summary, and per-probe logs.
  Preflight correctly reported `ready=false` because the only local
  `GramDrive.app` is the **unsigned dev assembly** in `.temp/app-packaging/`
  (fails `codesign --verify --deep --strict`) — an honest, correct finding, not
  a harness bug. `fileproviderctl dump` hangs on a host with no registered
  GramDrive domain; the harness's per-probe timeout (60 s) catches it and
  records it rather than wedging, and execution-level dedup means it runs once.
- No Rust or Swift source changed → core/apple builds unaffected; not rebuilt.
  No Python linter is configured in the repo/CI (the Python bar is the self-test
  suite, which passes).

## Deferred / for the human operator

1. On a **matrix Mac** with the **signed, notarized `GramDrive.app`** installed
   (`make package-app-notarize`) and a **Telegram test account** authorized:
   `make accept-macos`.
2. Follow `.temp/acceptance/<run-id>/runsheet.md` in Finder; **synthetic
   fixtures only** (NFR-005); GramDrive is **read-only** (NFR-014) — any
   successful Finder write is a failure.
3. Record PASS/FAIL + evidence per scenario in `evidence-template.md`; add
   screenshots.
4. Attach the whole `.temp/acceptance/<run-id>/` directory to the release task
   (AC: *results attach to release tasks*).

## Follow-ups noted (not in this task's scope)

- Wiring the harness's *prepare* mode (`--emit-runsheet` / a preflight-only run)
  into `native-ci.yml` so the run-sheet is proven to render on every
  release-branch PR, mirroring how the gate scripts are exercised. Left as CI
  wiring, which this repo keeps separate from the pipeline scripts.
- The sibling native-acceptance tasks (iOS `TASK-260715-27otl5`, Windows
  `TASK-260715-3uc2e9`, Android `TASK-260715-2uw5x8`, Linux
  `TASK-260715-eike3u`) can follow the same catalog-driven shape.
