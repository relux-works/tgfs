-- GramDrive state schema, version 15 (BUG-260728-15nihy).
--
-- Chat metadata remains the same generated-document appearance and therefore
-- keeps its provider item identity, render state, cache entry, pins, and
-- materialized bytes. Only its provider-visible filename becomes a dotfile.
--
-- Refresh the coalesced item journal around the update. Reconciliation will
-- subsequently be a no-op for this rename, so the migration itself must
-- publish the provider change for already-installed domains.

DELETE FROM item_changes
WHERE item_id IN (
    SELECT item_id
    FROM items
    WHERE kind = 'generated_doc'
      AND deleted_at_ms IS NULL
      AND display_name = 'chat.json'
      AND safe_name = 'chat.json'
);

UPDATE items
SET display_name = '.chat.json',
    safe_name = '.chat.json',
    metadata_version = 'hidden-chat-metadata-v15'
WHERE kind = 'generated_doc'
  AND deleted_at_ms IS NULL
  AND display_name = 'chat.json'
  AND safe_name = 'chat.json';

INSERT INTO item_changes (item_id, account_id)
SELECT item_id, account_id
FROM items
WHERE kind = 'generated_doc'
  AND deleted_at_ms IS NULL
  AND display_name = '.chat.json'
  AND safe_name = '.chat.json'
  AND metadata_version = 'hidden-chat-metadata-v15';
