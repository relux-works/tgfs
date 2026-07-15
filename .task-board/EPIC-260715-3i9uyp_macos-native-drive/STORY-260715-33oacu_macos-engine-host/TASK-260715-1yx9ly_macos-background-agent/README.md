# Implement companion agent lifecycle

## Description
Run TDLib/core with login/background/startup policy and bounded IPC/service interface.

## Scope
Launch, sleep/wake, crash, update, logout, and multiple accounts path.

## Acceptance Criteria
Agent recovers without duplicate work, exposes health, shuts down cleanly, and respects user launch preference.
