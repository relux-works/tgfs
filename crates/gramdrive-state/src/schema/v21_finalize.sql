-- Final swap for the resumable v21 shadow copy. This script and the
-- schema/version stamps commit together after row-count and foreign-key
-- validation in the migration runner.

DROP TABLE chat_list_entries;
ALTER TABLE chat_list_entries_v21 RENAME TO chat_list_entries;

CREATE INDEX chat_list_entries_order
    ON chat_list_entries (account_id, namespace_version, list_kind, folder_id, pinned DESC, sort_order DESC);
CREATE INDEX chat_list_entries_by_chat
    ON chat_list_entries (account_id, namespace_version, chat_id);

CREATE TRIGGER chat_list_entries_seed_sync_state
AFTER INSERT ON chat_list_entries
WHEN NEW.list_kind IN ('main', 'archive', 'folder')
BEGIN
    INSERT INTO chat_sync_state (
        account_id, namespace_version, chat_id,
        oldest_loaded_message_id, newest_loaded_message_id,
        history_complete, last_sync_at_ms
    ) VALUES (
        NEW.account_id, NEW.namespace_version, NEW.chat_id,
        NULL, NULL, 0, NULL
    ) ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;

    INSERT INTO chat_content_progress (
        account_id, namespace_version, chat_id, phase,
        failure_category, retryable, retry_at_ms, attempt_count, updated_at_ms
    )
    SELECT
        c.account_id, c.namespace_version, c.chat_id,
        CASE WHEN c.is_protected = 1 THEN 'protected' ELSE 'pending' END,
        CASE WHEN c.is_protected = 1 THEN 'protected-content' ELSE NULL END,
        0, NULL, 0, COALESCE(c.last_update_at_ms, 0)
    FROM chats c
    WHERE c.account_id = NEW.account_id
      AND c.namespace_version = NEW.namespace_version
      AND c.chat_id = NEW.chat_id
    ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;
END;

DROP TABLE items;
ALTER TABLE items_v21 RENAME TO items;

CREATE TRIGGER items_tombstone_provenance_insert
BEFORE INSERT ON items
FOR EACH ROW
WHEN (NEW.deleted_at_ms IS NULL AND NEW.tombstone_provenance IS NOT NULL)
     OR
     (NEW.deleted_at_ms IS NOT NULL AND COALESCE(
         NEW.tombstone_provenance NOT IN ('reconcile', 'retention', 'policy'),
         1
     ))
BEGIN
    SELECT RAISE(ABORT, 'items tombstone provenance invariant');
END;

CREATE TRIGGER items_tombstone_provenance_update
BEFORE UPDATE OF deleted_at_ms, tombstone_provenance ON items
FOR EACH ROW
WHEN (NEW.deleted_at_ms IS NULL AND NEW.tombstone_provenance IS NOT NULL)
     OR
     (NEW.deleted_at_ms IS NOT NULL AND COALESCE(
         NEW.tombstone_provenance NOT IN ('reconcile', 'retention', 'policy'),
         1
     ))
BEGIN
    SELECT RAISE(ABORT, 'items tombstone provenance invariant');
END;
