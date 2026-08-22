# Acceptance runners

Nine scripts live here, one per kind of acceptance the project gates on:

| Script | Kind | Runs unattended? |
|---|---|---|
| `run_automated.py` | The automated gate suites — format, lint, test, architecture, supply-chain, traceability, script self-tests, the macOS `swift build`/`swift test` leg, secret scan | **Yes.** This is CI's single entrypoint; documented in the top-level `README.md`. |
| `run_live_content.py` | Combined pre-install synthetic live-content matrix across focused Rust conformance/integration suites and the full Swift package/provider regression suite | **Yes.** Invoke through `run_automated.py --suite live-content`; it persists only fixed labels, counts, booleans, timings, versions, and bounds. |
| `run_installed_live_content.py` | Installed authorized-profile date-first enumeration, one fresh placeholder hydration, and relaunch comparison | **Yes, on the authorized local macOS profile.** Its public evidence excludes identifiers, paths, names, and content; raw comparison keys remain only in its temporary private state. |
| `run_installed_history_convergence.py` | Installed authorized-profile account-wide history convergence, bounded CPU/Finder enumeration, dataless-media, and relaunch comparison | **Yes, on the authorized local macOS profile.** Chat identities and cursor bounds remain only in its temporary private state; public evidence is aggregate-only. |
| `run_installed_index_metadata.py` | Installed authorized-profile chat-index convergence reach, directory size rollups, and correspondence dates, compared across an app + agent relaunch | **Yes, on the authorized local macOS profile.** Public evidence is counts, booleans, and byte totals only; cursor bounds live behind a per-run salted digest in a caller-chosen private directory. |
| `run_installed_foreground_demand.py` | Installed authorized-profile check that using a chat in Finder buys it a history turn, with no control-socket hint anywhere: reading one generated document inside it (`--gesture read`, the acceptance boolean) or only opening its folder (`--gesture open`, the platform-truth record) | **Yes, on the authorized local macOS profile.** Public evidence is counts, booleans, seconds, and message deltas only; the chosen chat's identity, the document read, and its raw cursor readings stay in a caller-chosen private file. |
| `run_installed_generated_hydration.py` | Bounded 20-read installed acceptance for dataless Markdown, NDJSON, and chat JSON under active backfill, repeated after relaunch | **Yes, on the authorized local macOS profile.** Every provider read has a killable deadline. Public evidence contains only aggregate latency, errno, timeout, hint, cursor, identity, and backfill facts; item paths, identifiers, and digests stay in caller-owned private state. |
| `run_installed_fault_recovery.py` | QA-only installed Open/content and Quick Look/thumbnail failure-preservation matrix | **Only in a dedicated QA macOS profile with the compile-time QA bundle.** Uses synthetic image fixtures; publishes no identities, paths, or content. |
| `run_native_macos.py` | The macOS **native manual acceptance** — the ten File Provider Finder flows the release gate requires | **No.** Human-in-the-loop by necessity (below). |

This README covers `run_native_macos.py`. For the gate runner, see the repo
`README.md` ("Running the checks").

## Synthetic live-content matrix

Run after staging the Swift core package (`make package` or
`make package-host-test`):

```sh
make check-live-content
```

The matrix covers history/live/story source conformance, deterministic monthly
rendering, Markdown/NDJSON, state fidelity/retention/100k query-plan bounds,
FFI hydration/policy, the Swift build, and all Swift provider/agent/companion
regressions. Every child process has a 900-second deadline. Its
`live-content.json` evidence is capped at 64 KiB, subprocess output is discarded,
and the schema rejects any field outside aggregate counts, booleans, timings,
tool versions, and fixed synthetic scenario labels.

The installed-profile runner is intentionally separate from CI. Run its
`before` phase against the installed Developer ID candidate, relaunch the
agent without resetting the profile, then run `after`. If unrelated live
discovery changes the global item set, run `stability-snapshot` after it
quiesces and relaunch again; this phase verifies and retains the exact private
sample identity, expected size, and digest captured by `before`, regardless of
newer unrelated cache entries, and never opens content. The first phase refuses
cached or non-dataless samples and re-checks both properties after enumeration
before its single attachment open. It also opens one current nonempty
`Messages.md`, `Messages.ndjson`, and hidden `.chat.json`, compares their exact bytes
with the verified generated cache, validates MIME/size/version/date facts, and
checks that all current generated references survived while no orphan
generation remains and physical generated bytes are within the configured
quota. The second phase repeats those checks after relaunch and compares the
sampled item, active item count/set, and each persisted cursor using the
product's monotonic window contract. Ongoing discovery may add items; evidence
reports the exact delta and distinguishes an identical set from additive-only
preservation of every prior identity. Public evidence contains aggregate
counts, sizes, and booleans only. Any required acceptance boolean that is false
makes the process exit nonzero.

For account-wide history, capture `run_installed_history_convergence.py before`
before installing the candidate, run `after` once background work has crossed
at least three independent listed chats, relaunch the agent without resetting
the profile, and run `relaunch`. The probe decodes only the frozen item-identity
prefix needed to correlate direct `YYYY-MM` appearances with their source chat.
It requires every currently stored source month in every terminal eligible chat
to have both clean, verified monthly exports; samples at least three independent
pre-current-history chats and repeatedly compares Finder-opened Markdown and
NDJSON bytes with the current managed materializations. It also requires
monotonic cursor windows, no new verified media blob, bounded agent CPU, and
responsive top-level Finder enumeration. Only the private state retains chat
keys, cursor bounds, and month membership.

For the historical chat index, run `run_installed_index_metadata.py before`
against the installed build, install the candidate, let background work run,
then `after`, then relaunch the app and agent without resetting the profile and
run `relaunch`. Its acceptance booleans are: every chat, month, and
`Active Stories` directory publishes a size rollup; every rollup equals the sum
of the indexed descendants the same database holds; no live directory reports
the epoch; no listed chat is unreachable by background history work; and every
cursor window is monotonic across the relaunch. The probe opens no content and
downloads nothing.

For the foreground-demand path, run `run_installed_foreground_demand.py`
against the installed candidate while the agent is running. It picks the
reachable incomplete chat the background rotation will reach *last* (highest
`last_backfill_at_ms`) that also has a folder on the mounted domain, watches it
for one window while touching nothing, performs one gesture on it, and watches
it for an equal window again. The gesture's acceptance boolean is true only when
the chat took a backfill turn after the gesture and took none during the control
window — a turn the rotation would have handed out anyway fails the probe rather
than passing it. It also reads the agent's `historyPriorityHints` counters
either side of the gesture, so a chat that did not advance can be attributed to
the provider not sending a hint or to the agent not honoring one; counters that
cannot be read are reported unobserved, never as zero.

Which gesture to run, and why there are two:

- `--gesture read` (default; `content_read_granted_a_turn`) reads one
  *generated* document inside the chat — `.chat.json`, or the smallest
  `Messages.md`/`Messages.ndjson` in a month. Generated documents are rendered
  from the index the agent already holds, so this downloads no Telegram payload
  bytes. A content read is the one interaction that reliably reaches the
  extension, so it is what the foreground claim is measured on.
- `--gesture open` (`foreground_open_granted_a_turn`) only lists the chat
  folder. On a replicated domain macOS answers a read of an
  already-materialized directory from its own copy of the namespace and never
  calls the extension's enumerator, so no hint is emitted and this gesture is
  expected to report `false` on a chat the rotation is not about to reach. That
  is the platform behavior, not a regression: **opening a chat folder does not
  prioritise it; opening something inside it does, and a folder-open-only
  interaction is served by the fair background rotation** (BUG-260728-2qfzbd).
  The gesture is kept runnable so that statement stays measured.

Its `before` phase deliberately asserts nothing — it is a measurement of the
build being replaced, and a pre-fix build is expected to fail every check.

For generated-document saturation, run
`run_installed_generated_hydration.py before` with task-scoped `--private` and
`--evidence` paths while the installed agent is actively backfilling. Relaunch
the app and agent without resetting the profile or File Provider domain, then
run `after` with the same private path and a new public evidence path. Each
phase selects 20 distinct, initially dataless documents: 7 Markdown, 7 NDJSON,
and 6 hidden chat JSON files, split across active pending/syncing chats and
history-complete chats. Every read runs in a child process with a 10-second
deadline and is compared byte-for-byte with its verified local cache entry.
The gate requires zero timeouts and errnos, p95 below 1 second, p99 below 3
seconds, balanced requested/background hint counters, a target chat turn,
monotonic cursors, and preserved account/domain/item identity across relaunch.

## BUG-260729-3uclm3 installed fault-recovery acceptance

This runner cannot arm an ordinary GramDrive candidate. The QA parser, fixed
record endpoint, and per-build authentication secret are compiled only by
`build_qa_fault_bundle.py`; ordinary packaging scrubs the build variables and
scans the assembled binaries to prove all three markers are absent. Neither
bundle uses `com.apple.developer.fileprovider.testing-mode`, a listener, or a
new entitlement.

Exact external procedure (not run on a preserved user profile):

1. Use a dedicated macOS QA user/profile. In a synthetic Telegram fixture chat,
   provide ten small PNG attachments named
   `gramdrive-qa-fault-{content,thumbnail}-{timeout,transport,renderer_source_not_found,source_not_found,unavailable_content}.png`.
2. From a clean task worktree, create a private per-build key and stage the
   real TDLib-linked core:

   ```bash
   umask 077
   openssl rand -hex -out .temp/BUG-260729-3uclm3_qa-secret 32
   GRAMDRIVE_TDLIB_ARTIFACT_DIR=.temp/tdlib/out make package
   ```

3. Build the non-shipping signed QA bundle. It is never notarized or published:

   ```bash
   python3 .scripts/apple-app/build_qa_fault_bundle.py \
     --secret-file .temp/BUG-260729-3uclm3_qa-secret \
     --out-dir .temp/BUG-260729-3uclm3_qa-package
   ```

4. Inspect `manifest.json`: `qa_fault_control.enabled` and
   `binary_boundary_verified` must both be true. Verify app/agent/appex
   entitlements contain no File Provider testing-mode entitlement. Install this
   QA artifact only in the dedicated QA profile, authorize the synthetic
   account, and wait until all ten fixture placeholders are visible and
   dataless.
5. Run the matrix:

   ```bash
   python3 .scripts/acceptance/run_installed_fault_recovery.py \
     --secret-file .temp/BUG-260729-3uclm3_qa-secret \
     --evidence .temp/BUG-260729-3uclm3_installed-fault-recovery.json
   ```

The runner arms one authenticated fault for one stable item and operation,
triggers `open -g` for the real content callback or `qlmanage -t` for the real
thumbnail callback, checks aggregate provider telemetry plus the durable row,
clears the record, and retries. Quick Look output is confined to a mode-0700
temporary directory and deleted immediately. Evidence contains only fixed fault
labels, counts, and booleans. A safe installed run remains an external action
because this developer run must not replace the preserved installed profile.

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
