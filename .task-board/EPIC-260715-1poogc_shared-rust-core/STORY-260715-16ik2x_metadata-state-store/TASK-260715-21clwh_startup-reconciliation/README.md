# Implement startup reconciliation and repair

## Description
Reconcile database, partial files, cache files, provider registrations, interrupted renders, and transfer journal.

## Scope
Automatic startup pass plus explicit user-triggered repair plan.

## Acceptance Criteria
Synthetic corruption/missing/extra fixtures converge without Telegram writes or loss of valid pinned content.
