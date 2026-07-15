# Message history and live updates

## Description
Implement resumable history crawl, normalized messages, incremental updates, gap recovery, edits, deletes, topics/albums/replies, and checkpoints.

## Scope
Metadata-first backfill; no eager media download.

## Acceptance Criteria
Idempotent replay and interruption tests pass; affected render partitions are signaled; unavailable history is explicit.
