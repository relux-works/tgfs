# Implement repair and diagnostic export

## Description
Provide reconciliation entrypoint, dry-run/plan, execution progress, and redacted support bundle.

## Scope
Database/cache/provider/source checks without Telegram mutation.

## Acceptance Criteria
Corruption fixtures repair or report precise unresolved state; bundle passes redaction tests and includes version/context.
