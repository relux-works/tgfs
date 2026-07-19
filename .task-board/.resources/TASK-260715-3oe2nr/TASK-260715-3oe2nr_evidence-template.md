# GramDrive — macOS native acceptance evidence & sign-off

| Field | Value |
|---|---|
| Run id | (fill in) |
| Commit | (fill in) |
| Operator | (name) |
| Date | (YYYY-MM-DD) |
| Build cdhash / version | (from manifest.json or `codesign -dv`) |
| Host (macOS / arch) | (e.g. macOS 14.5 / arm64) |
| Telegram test account | (identifier, synthetic) |

Fill one block per scenario. Verdict is PASS / FAIL / BLOCKED. Reference the captured probe logs (`<scenario>.<probe>.log`) and any screenshots you add to this directory.

## 1. Domain registration  (`registration`)

- Proves: the account's File Provider domain registers and appears in Finder's sidebar
- Spec: PLAT-MAC-001, SYNC-070, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **sidebar-entry** — expected: a GramDrive location for the account appears under Locations in the sidebar
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 2. Enumeration (dataless placeholders)  (`enumeration`)

- Proves: Finder shows the chat tree as stable dataless placeholders, no content hydrated by browsing
- Spec: PLAT-MAC-004, SYNC-003, SYNC-040, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **tree-visible** — expected: the Account/Main/<chat> tree renders; message/media items show as not-yet-downloaded (cloud/download badge), and browsing downloads nothing
    - [ ] **no-eager-hydrate** — expected: no file starts downloading merely from being listed (SYNC-040)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 3. Hydration of a dataless file  (`hydrate`)

- Proves: opening a dataless file streams the correct bytes and promotes it atomically
- Spec: PLAT-MAC-004, SYNC-041, SYNC-042, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **open-file** — expected: it downloads, opens, and its bytes/size match the known fixture (SYNC-042 atomic promote)
    - [ ] **materialized-after** — expected: the file now shows as downloaded/materialized, not a placeholder
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 4. Cancellation of an in-flight hydration  (`cancel`)

- Proves: cancelling a download stops promptly and leaves the item safely dataless, not corrupt
- Spec: PLAT-MAC-004, SYNC-043, SYNC-005, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **cancel-download** — expected: the download stops promptly; no partial file is presented as complete
    - [ ] **reopen-after-cancel** — expected: it hydrates cleanly from scratch (resumable/disposable state, SYNC-043)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 5. Offline pinning  (`pin`)

- Proves: pinning keeps content offline; it survives eviction pressure
- Spec: PLAT-MAC-004, SYNC-051, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **keep-downloaded** — expected: the file is marked always-kept-offline
    - [ ] **pin-survives** — expected: the pinned file stays materialized while unpinned content may evict (SYNC-051)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 6. Title/order change keeps stable identity  (`update`)

- Proves: a chat title/order change updates the appearance without breaking item identity
- Spec: PLAT-MAC-002, SYNC-026, SYNC-045, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **rename-observed** — expected: the Finder folder's display name/position updates to match
    - [ ] **identity-stable** — expected: the file is still materialized/pinned — its identity did not change (SYNC-026/045)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 7. Provider / app process restart  (`restart`)

- Proves: after a provider/app restart Finder shows stable placeholders and materialized state persists
- Spec: PLAT-MAC-001, SYNC-004, SYNC-031, NFR-004, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **restart-provider** — expected: the same chat tree and stable placeholders reappear (identity survives restart, SYNC-004)
    - [ ] **materialized-persists** — expected: it is still materialized/pinned; no re-download of already-local content
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 8. User-triggered domain repair  (`repair`)

- Proves: the companion "Repair File Provider Domains" action rebuilds provider state without data loss
- Spec: PLAT-MAC-004, SYNC-070, SYNC-071, NFR-034, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **run-repair** — expected: a lost account domain is re-registered and recovers its existing Finder state; strays are cleaned with downloads preserved (SYNC-071)
    - [ ] **no-total-teardown** — expected: repair refuses the total teardown and leaves domains in place (TotalTeardownPolicy.refuse)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 9. In-place upgrade  (`upgrade`)

- Proves: installing a newer signed build over the old one preserves domains, pins, and materialized state
- Spec: PLAT-004, NFR-013, SYNC-072, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **install-over** — expected: the app upgrades; the launch reconcile re-registers the same domains without wiping Finder state
    - [ ] **state-survives-upgrade** — expected: all survive; any DB/schema migration is transactional/resumable (NFR-013/SYNC-072)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## 10. Account removal / uninstall cleanup  (`remove`)

- Proves: removing the account (or uninstalling) tears the domain down cleanly with no orphan state
- Spec: PLAT-MAC-004, PLAT-004, SYNC-062, gate:macOS-spike
- **Verdict:** ______  (PASS / FAIL / BLOCKED)
- Operator checks:
    - [ ] **remove-account** — expected: the account's GramDrive location disappears from Finder; the domain is unregistered
    - [ ] **no-orphans** — expected: no orphan domain or mount for the removed account remains (clean removal)
- Evidence attached: ______ (probe logs, screenshots)
- Notes: ______

## Overall

- **Release-gate verdict:** ______ (all scenarios PASS / list failures)
- Known limitations recorded: ______
- Signed: ______  Date: ______
