# Acceptance runners

Two scripts live here, one per kind of acceptance the project gates on:

| Script | Kind | Runs unattended? |
|---|---|---|
| `run_automated.py` | The automated gate suites — format, lint, test, architecture, supply-chain, traceability, script self-tests, the macOS `swift build`/`swift test` leg, secret scan | **Yes.** This is CI's single entrypoint; documented in the top-level `README.md`. |
| `run_native_macos.py` | The macOS **native manual acceptance** — the ten File Provider Finder flows the release gate requires | **No.** Human-in-the-loop by necessity (below). |

This README covers `run_native_macos.py`. For the gate runner, see the repo
`README.md` ("Running the checks").

## What `run_native_macos.py` is for

`.spec/quality-and-release.md` gates a macOS release on *native manual
acceptance on the support matrix*, and the macOS File Provider spike gates spell
out the flows: Finder shows stable placeholders after restart, opening a
dataless file hydrates correct bytes and cancellation is safe, title/order
changes keep identity, provider/app restart and account removal are clean. This
script is the harness for that gate. It covers ten scenarios in run order:

1. **registration** — the account's File Provider domain registers and shows in Finder (PLAT-MAC-001, SYNC-070)
2. **enumeration** — the chat tree renders as dataless placeholders; browsing hydrates nothing (SYNC-003, SYNC-040)
3. **hydrate** — opening a dataless file streams the right bytes, promoted atomically (SYNC-041/042)
4. **cancel** — cancelling a download stops promptly, leaves no corrupt/partial file (SYNC-043, SYNC-005)
5. **pin** — "Keep Downloaded" survives eviction pressure (SYNC-051)
6. **update** — a chat title/order change updates the appearance without breaking identity (SYNC-026/045)
7. **restart** — after a provider/app restart, placeholders and materialized state persist (SYNC-004/031)
8. **repair** — the companion's "Repair File Provider Domains" rebuilds provider state without data loss (SYNC-070/071)
9. **upgrade** — installing a newer signed build over the old one preserves domains, pins, materialized state (NFR-013, SYNC-072)
10. **remove** — removing the account / uninstalling tears the domain down cleanly, no orphans (SYNC-062, PLAT-004)

## Why it is human-in-the-loop, and honest about it

The Finder flows cannot pass unattended today, and pretending they can would make
the gate lie. Three reasons:

- They need a **real signed, installed `GramDrive.app`**, a **Telegram test
  account**, and a **person watching Finder** — a screenshot of a placeholder
  badge is a human judgement, not a script's.
- TDLib is **not yet linked into `gramdrive-agent`** (`.scripts/apple-app/README.md`),
  so even "open a dataless file and get the right bytes" has no unattended path
  yet.

So the harness automates only what a machine can *truthfully* check, and prepares
the rest for the operator. **It never reports a scenario as passed** — a run it
completes is `prepared`, awaiting the human sign-off the release gate actually
accepts. The task checklist forbids self-closing this gate.

## What the harness does

One **scenario catalog** in the script is the single source of truth for the
run-sheet, the evidence form, and the probes — one edit moves all three, so they
never drift. For a run it:

1. **Preflights** the host against the v1 matrix (macOS 14+ arm64, POL-5/DEC-017):
   OS version, arch, a located `GramDrive.app`, its Developer ID signature
   (`codesign --verify --deep --strict`), Gatekeeper verdict (recorded, not
   gated — an un-notarized Developer ID build is legitimately rejected),
   `fileproviderctl` availability, App Group container. Every finding is
   recorded with its reason.
2. **Probes** each scenario: machine assertions only where robust (the provider
   domain is registered; the signature is valid), and evidence capture
   otherwise (`fileproviderctl dump`, the `com.reluxworks.gramdrive` unified
   log, `codesign`, `stat`). Identical commands across scenarios execute once.
3. **Emits** into `.temp/acceptance/<run-id>/`:
   - `runsheet.md` — the operator's step-by-step Finder run-sheet with expected outcomes per scenario
   - `evidence-template.md` — the fill-in evidence + sign-off form
   - `summary.json` — the environment, per-scenario machine verdicts (never "passed"), and probe results, attributable to a commit (NFR-052)
   - `<scenario>.<probe>.log` and `preflight.<probe>.log` — captured evidence

## Running it

```sh
make accept-macos                                        # prepare a run on this host
make accept-macos-runsheet                               # render the run-sheet to stdout (no host needed)
python3 .scripts/acceptance/run_native_macos.py --list   # print the scenario catalog
python3 .scripts/acceptance/run_native_macos.py --run-id accept-2026-07-19
python3 .scripts/acceptance/run_native_macos.py --run-id accept-2026-07-19 --app-path /Applications/GramDrive.app
python3 .scripts/acceptance/run_native_macos.py --emit-evidence-template evidence.md
```

`--require-ready` makes the harness exit 3 unless the host is the gate matrix
with a located, valid-signed build — for a wrapper that wants to refuse a run on
the wrong machine. Without it, a run always *prepares* (exit 0): even on a dev
box or CI without a signed build it writes the run-sheet and the evidence form,
it just cannot run the live probes.

Exit codes: `0` prepared (not a pass claim) · `2` could not start (bad args /
broken catalog) · `3` `--require-ready` and the host is not the matrix.

## The operator flow

1. On a matrix Mac with the signed build installed and a Telegram test account
   authorized, run `make accept-macos`.
2. Open `.temp/acceptance/<run-id>/runsheet.md` and work through each scenario in
   Finder. **Synthetic fixtures only** (NFR-005); GramDrive is **read-only**
   (NFR-014) — any successful Finder write is a failure.
3. Record PASS/FAIL + evidence per scenario in `evidence-template.md`; add
   screenshots to the run directory.
4. Attach the whole `.temp/acceptance/<run-id>/` directory to the release task
   (the AC: *results attach to release tasks*).

## Self-tests

`.scripts/tests/test_run_native_macos.py`, run by the `repo` gate suite. They
inject a fake command runner and filesystem oracle, so they cover the harness on
a machine without a Mac or a build — the catalog is the gate's ten scenarios,
the preflight classifies matrix vs non-matrix hosts, the probes assert only what
they can, and — the property that keeps the gate honest — a prepared run never
reports a scenario as passed.
