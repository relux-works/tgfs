# Implement service metadata schema

## Description
Store accounts, chats/appearances, messages, attachments, item versions, change journal, jobs, tokens, and retention state.

## Scope
Indexes, transactions, migrations, tenant/account keys, and audit metadata.

## Acceptance Criteria
Large synthetic account queries are bounded; idempotent ingest and durable cursors pass crash tests.
