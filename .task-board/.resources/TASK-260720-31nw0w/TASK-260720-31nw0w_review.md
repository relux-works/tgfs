# TASK-260720-31nw0w — macos-onboarding-flow — REVIEW VERDICT

**Verdict:** ACCEPTED → `done`
**Reviewer:** reviewer (claude)
**Date:** 2026-07-20

## What was reviewed
First-launch onboarding wizard for the GramDrive companion shell (SwiftUI, macOS 14+),
addressing v0.1.0 acceptance feedback ("after launch it is unclear what started —
no window, no guidance").

Files reviewed:
- `Onboarding/OnboardingViewModel.swift`, `Onboarding/DriveLocation.swift`,
  `Onboarding/OnboardingCompletionStore.swift`
- `Views/OnboardingView.swift`, `Views/MenuBarStatusView.swift`
- `CompanionViewModel.swift` (owns `onboarding`), `GramDriveCompanionMain/CompanionMain.swift`
- Tests: `OnboardingViewModelTests.swift`, `OnboardingSeamTests.swift`

## Independent verification (not taken on faith)
- `swift test` full apple package: **304 tests / 56 suites passed**, 0 failures, **0 warnings**.
- Onboarding suites re-run in isolation: **29 tests / 7 suites passed**
  (OnboardingPresentation/Navigation/SignInStart/DriveLocation + InitialSyncDerivation +
  CompletionStore + DriveLocation seam).
- Deployment target confirmed `macOS(.v14)` (POL-5/DEC-017).
- `LSUIElement = True` is set by the app-bundle packaging script and guarded by a test —
  the menu-bar-only design the first-launch trigger relies on is real.
- Blocking edge `BUG-260720-3i74u1` (companion↔agent control channel) is `done`, so the
  sign-in step's live-channel dependency is satisfied.
- POL-2: default cache quota resolves to 10 GB; Archive Mode `false` by default with
  correct cloud-placeholder copy. POL-1 copy (chats as folders / cloud-on-open) present.

Signed packaging (`make package-app`, Developer ID) is CI/keychain-gated; not re-run here.
Local proof is the clean compile + full green suite; implementer reports the signed
assembly and `codesign --verify --deep --strict` passed.

## AC coverage
- Welcome window: app glyph + one-line value prop + 3-point plan — OK
- Guided steps Sign In → Defaults (cache quota, Archive Mode off per POL-2, launch-at-login)
  → Success (Open in Finder + live initial-sync progress) — OK
- Sign-in embeds the live `AuthorizationView` over the shared `AuthorizationViewModel`,
  gated on `isAuthorized`, honest "unavailable" state preserved — OK
- Menu-bar present from first launch; click opens the compact `MenuBarStatusView` — OK
- Shown once (persisted `UserDefaults` flag), re-runnable from Help ▸ Setup Guide and the
  compact panel — OK
- SwiftUI, macOS 14+, screens covered by unit tests — OK

## Architecture fit
Strong. Onboarding drives the SAME live auth/settings/status sub-view-models the shell
uses — no parallel sign-in/settings paths, so state is consistent when the flow ends.
Testability seams (`OnboardingCompletionStore`, `DriveLocationProviding`) and pure
projections (`InitialSyncStatus`, honest "waiting/preparing/syncing(N)/upToDate" from
agent health) match the codebase's established honesty rule. New `CompanionViewModel`
params are defaulted, so existing callers/previews are untouched. SwiftUI usage is sound:
`Window` singleton opened via `openWindow(id:)`, `MenuBarExtra(.window)`, `@Bindable` on
injected observables, `.task` cancellation on the success-step poll loop, accessibility
labels/hidden glyphs.

## Minor observations (non-blocking, no rework required)
1. Menu-bar icon is a static glyph; it does not reflect running/syncing state. "Showing
   status" is delivered by the compact panel on click. Reflecting state in the icon would
   be a nice enhancement.
2. The onboarding Defaults Archive-Mode toggle has no disk preflight row (StorageSettingsView
   shows a projection / low-disk warning). Not a POL-2 violation — the main toggle isn't
   hard-gated either, and Archive Mode defaults OFF — but parity would be nicer.
3. First-launch auto-open rides the always-present MenuBarExtra label's `.task`; it is not
   unit-testable and cannot be proven without a clean-machine launch. The presentation
   *decision* is fully tested, and Help + panel are guaranteed manual fallbacks. Design is
   sound.
4. Navigating Back from Defaults then forward reloads persisted settings (discards unsaved
   edits on that revisit) — standard behavior.

None of these block acceptance.
