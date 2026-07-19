# GramDrive — macOS native acceptance run-sheet


This is the release-gate manual acceptance for the macOS File Provider drive (`.spec/quality-and-release.md`, macOS spike gates). A person runs it on a **real signed, installed `GramDrive.app`** with a **Telegram test account**; the harness has already captured the machine-checkable probes and the environment preflight into this run's directory.

## Ground rules

- **Synthetic fixtures only** (NFR-005): use a dedicated Telegram test account, never real personal data.
- **Read-only** (NFR-014, SYNC-060): GramDrive never writes to or deletes from Telegram. If any Finder write *succeeds*, that is a failure.
- Matrix: **macOS 14+ arm64** (POL-5/DEC-017). Run on the support matrix, not a VM that misreports either.
- For every scenario, attach the referenced evidence and record PASS/FAIL + notes in `evidence-template.md`.

## Scenarios

### 1. Domain registration  (`registration`)

- **Proves:** the account's File Provider domain registers and appears in Finder's sidebar
- **Spec:** PLAT-MAC-001, SYNC-070, gate:macOS-spike
- **Preconditions:**
    - GramDrive.app installed and launched at least once
    - one Telegram test account authorized in the companion
- **Harness probes (already captured for you):**
    - `domain-present` (assert) — the GramDrive provider domain is registered with the system
        - `fileproviderctl dump`
    - `domain-log` (evidence) — capture the domain reconcile/repair log (com.reluxworks.gramdrive / file-provider-domains)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive" && category == "file-provider-domains"`
- **Operator steps (Finder):**
    1. **sidebar-entry** — open Finder after authorizing the test account
        - Expected: a GramDrive location for the account appears under Locations in the sidebar

### 2. Enumeration (dataless placeholders)  (`enumeration`)

- **Proves:** Finder shows the chat tree as stable dataless placeholders, no content hydrated by browsing
- **Spec:** PLAT-MAC-004, SYNC-003, SYNC-040, gate:macOS-spike
- **Preconditions:**
    - registration scenario passed
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
    - `cloudstorage-root` (evidence) — list the File Provider mount root and its dataless markers
        - `sh -c ls -la@O ~/Library/CloudStorage 2>&1 || true`
- **Operator steps (Finder):**
    1. **tree-visible** — browse the GramDrive location down to a chat folder
        - Expected: the Account/Main/<chat> tree renders; message/media items show as not-yet-downloaded (cloud/download badge), and browsing downloads nothing
    2. **no-eager-hydrate** — watch the download indicator while scrolling a large chat folder
        - Expected: no file starts downloading merely from being listed (SYNC-040)

### 3. Hydration of a dataless file  (`hydrate`)

- **Proves:** opening a dataless file streams the correct bytes and promotes it atomically
- **Spec:** PLAT-MAC-004, SYNC-041, SYNC-042, gate:macOS-spike
- **Preconditions:**
    - enumeration scenario passed
    - a known synthetic fixture file in the account
- **Harness probes (already captured for you):**
    - `pre-open-dataless` (evidence) — placeholder: operator records `stat -f '%Sf' <file>` before opening to show the dataless flag; captured in the evidence form
        - `sh -c true`
    - `hydration-log` (evidence) — capture the full GramDrive unified log for this window (com.reluxworks.gramdrive, all categories)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive"`
- **Operator steps (Finder):**
    1. **open-file** — double-click a known dataless media file (e.g. a fixture image)
        - Expected: it downloads, opens, and its bytes/size match the known fixture (SYNC-042 atomic promote)
    2. **materialized-after** — check the file's Finder badge after it opens
        - Expected: the file now shows as downloaded/materialized, not a placeholder

### 4. Cancellation of an in-flight hydration  (`cancel`)

- **Proves:** cancelling a download stops promptly and leaves the item safely dataless, not corrupt
- **Spec:** PLAT-MAC-004, SYNC-043, SYNC-005, gate:macOS-spike
- **Preconditions:**
    - a large synthetic fixture file whose download is slow enough to cancel
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump-after-cancel` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
    - `cancel-log` (evidence) — capture the full GramDrive unified log for this window (com.reluxworks.gramdrive, all categories)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive"`
- **Operator steps (Finder):**
    1. **cancel-download** — start downloading a large file, then cancel it from Finder's progress UI
        - Expected: the download stops promptly; no partial file is presented as complete
    2. **reopen-after-cancel** — open the same file again after cancelling
        - Expected: it hydrates cleanly from scratch (resumable/disposable state, SYNC-043)

### 5. Offline pinning  (`pin`)

- **Proves:** pinning keeps content offline; it survives eviction pressure
- **Spec:** PLAT-MAC-004, SYNC-051, gate:macOS-spike
- **Preconditions:**
    - a materialized file from the hydrate scenario
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump-pins` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
- **Operator steps (Finder):**
    1. **keep-downloaded** — right-click a materialized file and choose "Keep Downloaded" (pin)
        - Expected: the file is marked always-kept-offline
    2. **pin-survives** — trigger cache pressure / eviction (or the companion's evict action) and re-check the pinned file
        - Expected: the pinned file stays materialized while unpinned content may evict (SYNC-051)

### 6. Title/order change keeps stable identity  (`update`)

- **Proves:** a chat title/order change updates the appearance without breaking item identity
- **Spec:** PLAT-MAC-002, SYNC-026, SYNC-045, gate:macOS-spike
- **Preconditions:**
    - a chat visible in Finder whose Telegram title or list position can change
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump-before-update` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
    - `update-log` (evidence) — capture the domain reconcile/repair log (com.reluxworks.gramdrive / file-provider-domains)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive" && category == "file-provider-domains"`
- **Operator steps (Finder):**
    1. **rename-observed** — change the chat's Telegram title (or its dialog order) and let the change sync
        - Expected: the Finder folder's display name/position updates to match
    2. **identity-stable** — confirm a previously materialized or pinned file inside that chat after the rename
        - Expected: the file is still materialized/pinned — its identity did not change (SYNC-026/045)

### 7. Provider / app process restart  (`restart`)

- **Proves:** after a provider/app restart Finder shows stable placeholders and materialized state persists
- **Spec:** PLAT-MAC-001, SYNC-004, SYNC-031, NFR-004, gate:macOS-spike
- **Preconditions:**
    - registration + at least one materialized/pinned file
- **Harness probes (already captured for you):**
    - `agent-launchctl` (evidence) — capture the companion agent's launchd state before/after restart
        - `sh -c launchctl print gui/$(id -u)/com.reluxworks.gramdrive.agent 2>&1 | head -40 || true`
    - `fileproviderctl-dump-after-restart` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
- **Operator steps (Finder):**
    1. **restart-provider** — quit and relaunch GramDrive (or reboot); reopen the GramDrive location in Finder
        - Expected: the same chat tree and stable placeholders reappear (identity survives restart, SYNC-004)
    2. **materialized-persists** — check a file that was materialized/pinned before the restart
        - Expected: it is still materialized/pinned; no re-download of already-local content

### 8. User-triggered domain repair  (`repair`)

- **Proves:** the companion "Repair File Provider Domains" action rebuilds provider state without data loss
- **Spec:** PLAT-MAC-004, SYNC-070, SYNC-071, NFR-034, gate:macOS-spike
- **Preconditions:**
    - registration passed; ideally a domain the system has lost or a stray to clean
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump-before-repair` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
    - `repair-log` (evidence) — capture the domain reconcile/repair log (com.reluxworks.gramdrive / file-provider-domains)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive" && category == "file-provider-domains"`
- **Operator steps (Finder):**
    1. **run-repair** — invoke the companion menu action "Repair File Provider Domains…"
        - Expected: a lost account domain is re-registered and recovers its existing Finder state; strays are cleaned with downloads preserved (SYNC-071)
    2. **no-total-teardown** — observe repair when the canonical account set reads empty while a domain is still registered
        - Expected: repair refuses the total teardown and leaves domains in place (TotalTeardownPolicy.refuse)

### 9. In-place upgrade  (`upgrade`)

- **Proves:** installing a newer signed build over the old one preserves domains, pins, and materialized state
- **Spec:** PLAT-004, NFR-013, SYNC-072, gate:macOS-spike
- **Preconditions:**
    - an older signed GramDrive.app installed with materialized/pinned content
    - a newer signed build to install over it
- **Harness probes (already captured for you):**
    - `app-version-before` (evidence) — record the installed app version before the upgrade
        - `sh -c defaults read '/Applications/GramDrive.app/Contents/Info' CFBundleShortVersionString 2>&1 || true`
    - `fileproviderctl-dump-before-upgrade` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
- **Operator steps (Finder):**
    1. **install-over** — install the newer signed build over the existing one and relaunch
        - Expected: the app upgrades; the launch reconcile re-registers the same domains without wiping Finder state
    2. **state-survives-upgrade** — check domains, pins, and materialized files after the upgrade
        - Expected: all survive; any DB/schema migration is transactional/resumable (NFR-013/SYNC-072)

### 10. Account removal / uninstall cleanup  (`remove`)

- **Proves:** removing the account (or uninstalling) tears the domain down cleanly with no orphan state
- **Spec:** PLAT-MAC-004, PLAT-004, SYNC-062, gate:macOS-spike
- **Preconditions:**
    - registration passed for the account being removed
- **Harness probes (already captured for you):**
    - `fileproviderctl-dump-after-remove` (evidence) — capture current File Provider domain + item state
        - `fileproviderctl dump`
    - `cloudstorage-after-remove` (evidence) — confirm the provider mount for the removed account is gone
        - `sh -c ls -la ~/Library/CloudStorage 2>&1 || true`
    - `remove-log` (evidence) — capture the domain reconcile/repair log (com.reluxworks.gramdrive / file-provider-domains)
        - `log show --last 1h --style compact --predicate subsystem == "com.reluxworks.gramdrive" && category == "file-provider-domains"`
- **Operator steps (Finder):**
    1. **remove-account** — remove the test account from the companion (or uninstall the app)
        - Expected: the account's GramDrive location disappears from Finder; the domain is unregistered
    2. **no-orphans** — re-check fileproviderctl dump and ~/Library/CloudStorage after removal
        - Expected: no orphan domain or mount for the removed account remains (clean removal)

## Sign-off

Record each scenario's verdict and evidence in `evidence-template.md`, then attach this run's directory to the release task. A scenario passes only when its operator checks are confirmed — the harness does not and cannot pass them for you.
