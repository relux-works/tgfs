# TASK-260720-31nw0w: macos-onboarding-flow

## Description
Acceptance feedback on v0.1.0: after launch it is unclear what started - no window, no guidance. Build a classic macOS onboarding: first-launch Welcome window (app icon, one-line value prop: your Telegram chats as folders in Finder), guided steps: 1) Sign in to Telegram (embeds the auth flow from the control channel), 2) choose defaults (cache quota, Archive Mode off by default per POL-2, launch-at-login toggle), 3) success step showing where the drive lives (Open in Finder button to the CloudStorage location) with initial sync progress. Menu-bar item present from first launch with status; reopening the app or clicking the menu-bar icon after onboarding opens the compact status window. Onboarding shows once (persisted flag), re-runnable from Help menu. Respect POL-1/POL-2 semantics in copy. SwiftUI, macOS 14+, load the swiftui skill.

## Scope
(define task scope)

## Acceptance Criteria
On a clean machine the released app opens the Welcome window on first launch and walks sign-in through to chats visible in Finder with an Open in Finder affordance and visible sync progress; menu-bar presence from first launch; onboarding shown once, re-runnable from Help; screens covered by unit or view tests; swift test and make check green.
