# Implement resumable per-chat history crawl

## Description
Page normal TDLib history into normalized records with durable per-chat cursors and bounded scheduling.

## Scope
Private chats, groups, supergroups, channels, topics, and left/unsupported conditions.

## Acceptance Criteria
Restart continues without duplicates; flood waits are honored; priority favors visible/requested chats; huge histories remain bounded.
