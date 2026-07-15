# Implement durable transfer state machine

## Description
Model requested/completed ranges, source version, priority, retries, temporary data, cancellation, and terminal outcomes.

## Scope
Backend-neutral downloads and provider progress.

## Acceptance Criteria
State machine rejects invalid transitions, resumes after crash, and never exposes incomplete content as valid.
