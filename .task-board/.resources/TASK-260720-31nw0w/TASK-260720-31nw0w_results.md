# TASK-260720-31nw0w — macOS onboarding flow

**Status:** ready for review
**Scope:** First-launch Welcome/onboarding for the GramDrive companion shell
(SwiftUI, macOS 14+), addressing v0.1.0 acceptance feedback ("after launch it is
unclear what started — no window, no guidance").

## What shipped

A classic macOS onboarding wizard driven by the *same* live view models the
companion shell already uses, so sign-in genuinely authorizes over the agent
control channel and the defaults the user picks persist to the one settings
document the agent reads.

Steps: **Welcome → Sign In → Choose Defaults → You're All Set.**

- **Welcome** — app glyph, one-line value prop ("Your Telegram chats as folders
  in Finder"), a 3-point plan, and a POL-2 line (chats stay in the cloud and
  download on open; Archive Mode is opt-in).
- **Sign In** — embeds the existing live-control-channel `AuthorizationView`
  (phone/code/QR/2FA), auto-started on entry. The step gates: the flow does not
  advance until the account is authorized. Honest "control channel unavailable"
  state is preserved (not re-begun over).
- **Choose Defaults** — cache quota stepper, Archive Mode toggle (off by default
  per POL-2, with cloud-placeholder copy), launch-at-login toggle (reconciled
  through the real `SMAppService` login item in production). Values persist on
  Continue.
- **You're All Set** — resolves the drive's Finder location, an **Open in
  Finder** button, and a live **initial-sync** indicator projected purely from
  agent health (waiting → preparing → syncing(N) → up to date). Polls health
  every 2s while shown.

**Menu-bar item from first launch:** the `MenuBarExtra` now shows a compact
`MenuBarStatusView` (agent/account/sync at a glance + actions) instead of the
full split-view. Full shell stays behind "Open GramDrive…".

**Shown once / re-runnable:** completion is a persisted `UserDefaults` flag.
On a clean machine onboarding auto-opens on launch; once finished or explicitly
skipped it never auto-shows again; re-runnable from **Help ▸ GramDrive Setup
Guide** and the compact panel's "Setup Guide…".

## Files

New (in `GramDriveCompanion`):
- `Onboarding/OnboardingCompletionStore.swift` — persisted "shown once" flag
  (protocol + `UserDefaults` + in-memory).
- `Onboarding/DriveLocation.swift` — `DriveLocationProviding` seam +
  `CloudStorageDriveLocation` (resolves `~/Library/CloudStorage/GramDrive*`,
  falls back to the container) + `FixedDriveLocation` for tests.
- `Onboarding/OnboardingViewModel.swift` — step navigation, sign-in gating,
  finish/skip/restart, `InitialSyncStatus` pure projection.
- `Views/OnboardingView.swift` — the wizard window + four step views.
- `Views/MenuBarStatusView.swift` — the compact menu-bar panel.

Changed:
- `CompanionViewModel.swift` — owns `onboarding`, wired over the shared
  auth/settings/status sub-models; new `driveLocation` / `onboardingStore`
  params (defaulted, so existing callers/previews are unaffected).
- `GramDriveCompanionMain/CompanionMain.swift` — compact menu-bar content,
  onboarding `Window`, first-launch presentation via the always-present
  menu-bar label, Help re-run command.

Tests (in `GramDriveCompanionTests`):
- `OnboardingViewModelTests.swift` — presentation seeding, navigation, sign-in
  gating (driven to `.ready` through a scripted session), defaults
  load/persist, finish/skip/restart, `beginSignInIfNeeded`, initial-sync
  derivation for every readout, drive reveal through the seam.
- `OnboardingSeamTests.swift` — completion store round-trips, CloudStorage
  resolution/fallback/ordering, reveal side-effect wiring.

## Verification

- `swift test` (full apple package): **304 tests / 56 suites passed** (79 in
  `GramDriveCompanionTests`, incl. the new onboarding suites).
- `swift build --product gramdrive-companion`: clean, no warnings.
- `make package-app-unsigned`: **APP PACKAGING PASSED** (release
  `gramdrive-companion` assembled into `GramDrive.app`).
- `make package-app` (signed): **APP PACKAGING PASSED** — Developer ID signed
  (`Relux Works, LLC 262RZ595FP`), `codesign --verify --deep --strict` ok, dmg
  produced. `spctl: rejected` is expected for an un-notarized Developer ID app
  (notarization is a separate release step).
- `make check` (Rust core + repo suite): **8/8 passed** (toolchain, format,
  clippy `-D warnings`, cargo test, architecture, supply-chain, traceability,
  scripts).

## Design decisions / notes

- **Reuse over re-implement:** onboarding drives the shell's existing live
  sub-models, so no parallel auth/settings paths and no drift.
- **Drive location** resolved via `~/Library/CloudStorage` + name prefix rather
  than a `NSFileProviderManager` dependency — keeps `GramDriveCompanion` free of
  FileProvider specifics and fully testable; falls back to the CloudStorage
  container before any provider folder exists.
- **Initial sync is honest:** projected from health fields
  (`pendingTransferCount`, `lastSourceUpdateMs`) — no fabricated percentage, per
  the codebase's "don't invent readings" rule.
- **Completion semantics:** recorded on finish/skip. A bare window-close (red
  button) does not record completion, so an accidental close re-nudges next
  launch rather than stranding a user who set nothing up.
- **First-launch auto-open** rides the always-present menu-bar label's `.task`
  (the one reliably-instantiated view at launch in an `LSUIElement` shell);
  Help + the compact panel are guaranteed manual entry points. Signing itself
  is a CI-only step (Developer ID identity); local proof is the unsigned
  assembly gate.
