# Implement metadata-first local backfill scheduler

## Description
Coordinate chat/history discovery, visible-item priority, flood waits, device power/network constraints, and optional desktop deep backfill.

## Scope
Normal TDLib API only; Takeout belongs to optional remote/import tooling.

## Acceptance Criteria
Scheduler is durable, bounded, observable, user-pausable, and avoids eager mobile media mirroring.
