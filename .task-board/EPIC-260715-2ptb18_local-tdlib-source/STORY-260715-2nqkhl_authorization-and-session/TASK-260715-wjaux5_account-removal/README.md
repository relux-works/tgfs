# Implement account logout and local removal workflow

## Description
Close TDLib, unregister provider state, cancel transfers, remove or retain cached exports per explicit user choice, and delete credentials.

## Scope
Crash-resumable destructive workflow with confirmation boundaries.

## Acceptance Criteria
Every stage is idempotent; partial failure resumes; Telegram logout versus local-only removal is clearly distinguished.
