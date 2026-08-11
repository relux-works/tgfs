-- GramDrive state schema, version 16 (BUG-260728-2qfzbd).
--
-- Two provider-visible facts that the date-first namespace could not carry:
--
-- 1. `items.aggregate_size` — the exact logical size of a directory's
--    indexed descendants. The v1 CHECK forbids `logical_size` on a
--    directory, and that stays true: `logical_size` remains "this file's own
--    bytes". A directory's rollup is a separate, separately-owned column, so
--    a file can never accidentally claim one and a directory can never
--    accidentally claim file content facts.
--
-- 2. The one-time rollup backfill for namespaces created before this
--    version. Chat and month directories already exist with stable item
--    identifiers; only the new column is filled, so no identifier, name,
--    parent, or content version changes here.
--
-- Correspondence-derived directory timestamps are deliberately *not*
-- backfilled by this script: a month partition is a local civil month under
-- the account's display timezone, which SQL cannot derive. The projection
-- owns those and rewrites every chat and month directory idempotently on the
-- next reconciliation pass, keeping one owner for the fact.

ALTER TABLE items ADD COLUMN aggregate_size INTEGER;

-- Month directories roll up their own live children.
UPDATE items
SET aggregate_size = COALESCE((
        SELECT sum(child.logical_size)
        FROM items AS child
        WHERE child.parent_item_id = items.item_id
          AND child.deleted_at_ms IS NULL
    ), 0)
WHERE kind IN ('month_dir', 'active_stories')
  AND deleted_at_ms IS NULL;

-- Chat directories roll up their month directories plus their own direct
-- files (the hidden chat-metadata document).
UPDATE items
SET aggregate_size = COALESCE((
        SELECT sum(COALESCE(child.aggregate_size, child.logical_size))
        FROM items AS child
        WHERE child.parent_item_id = items.item_id
          AND child.deleted_at_ms IS NULL
    ), 0)
WHERE kind = 'chat'
  AND deleted_at_ms IS NULL;

-- The rollup is a provider-visible attribute of an already-published item.
-- Republish the affected directories so an installed domain refreshes them
-- without an account reset. Reconciliation stamps the matching metadata
-- version on its next pass; the journal entry is what makes the system ask.
-- The delete/insert pair is the journal's coalescing discipline: a change
-- re-entered at a *newer* sequence is the only kind a provider already past
-- the old sequence will fetch again.
DELETE FROM item_changes
WHERE item_id IN (
    SELECT item_id
    FROM items
    WHERE kind IN ('chat', 'month_dir', 'active_stories')
      AND deleted_at_ms IS NULL
      AND aggregate_size IS NOT NULL
);

INSERT INTO item_changes (item_id, account_id)
SELECT item_id, account_id
FROM items
WHERE kind IN ('chat', 'month_dir', 'active_stories')
  AND deleted_at_ms IS NULL
  AND aggregate_size IS NOT NULL;
