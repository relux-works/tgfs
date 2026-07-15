# Implement service credential key management

## Description
Encrypt Telegram sessions and product secrets with a documented provider/rotation/backup strategy.

## Scope
Self-hosted baseline and hosted extension, operator access, disaster recovery, and incident response.

## Acceptance Criteria
Keys are never plaintext at rest/logged; rotation/restore tests pass; hosted mode has approved access controls.
