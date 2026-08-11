-- GramDrive state schema, version 13 (BUG-260727-2ct7o0).
--
-- Canonical chat metadata is broader than the provider namespace: TDLib may
-- retain thousands of chats that have no live Main, Archive, or custom-folder
-- position. History is useful only once a chat has at least one such
-- appearance. Seed traversal state at that eligibility boundary instead of
-- turning every canonical metadata row into runnable background work.
--
-- Existing cursor rows are deliberately preserved. A chat that leaves every
-- list may later reappear, and its contiguous history window must resume
-- without regression. The scheduler query owns current eligibility.

DROP TRIGGER IF EXISTS chats_seed_sync_state;

-- Bring forward any eligible chat that predates this migration but lacks a
-- cursor (for example, a partially restored database). Existing windows and
-- terminal cursors are never rewritten.
INSERT INTO chat_sync_state (
    account_id, namespace_version, chat_id,
    oldest_loaded_message_id, newest_loaded_message_id,
    history_complete, last_sync_at_ms
)
SELECT c.account_id, c.namespace_version, c.chat_id, NULL, NULL, 0, NULL
FROM chats c
WHERE EXISTS (
    SELECT 1
    FROM chat_list_entries e
    WHERE e.account_id = c.account_id
      AND e.namespace_version = c.namespace_version
      AND e.chat_id = c.chat_id
)
ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;

INSERT INTO chat_content_progress (
    account_id, namespace_version, chat_id, phase,
    failure_category, retryable, retry_at_ms, attempt_count, updated_at_ms
)
SELECT
    c.account_id, c.namespace_version, c.chat_id,
    CASE
        WHEN c.is_protected = 1 THEN 'protected'
        WHEN s.history_complete = 1 THEN 'ready'
        ELSE 'pending'
    END,
    CASE WHEN c.is_protected = 1 THEN 'protected-content' ELSE NULL END,
    0, NULL, 0, COALESCE(s.last_sync_at_ms, c.last_update_at_ms, 0)
FROM chats c
JOIN chat_sync_state s
  ON s.account_id = c.account_id
 AND s.namespace_version = c.namespace_version
 AND s.chat_id = c.chat_id
WHERE EXISTS (
    SELECT 1
    FROM chat_list_entries e
    WHERE e.account_id = c.account_id
      AND e.namespace_version = c.namespace_version
      AND e.chat_id = c.chat_id
)
ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;

CREATE TRIGGER chat_list_entries_seed_sync_state
AFTER INSERT ON chat_list_entries
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
