-- GramDrive state schema, version 5 (TASK-260721-yrcjlo).
--
-- Per-chat content progress is deliberately separate from chat_sync_state:
-- the latter is the correctness cursor and must remain a compact statement
-- of the normalized contiguous window. This table is the privacy-safe
-- operational state that lets enumeration distinguish pending work from a
-- protected, unavailable, failed, degraded, or cancelled chat.

CREATE TABLE chat_content_progress (
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id           INTEGER NOT NULL,
    phase             TEXT    NOT NULL CHECK (phase IN (
        'pending', 'syncing', 'ready', 'unavailable',
        'protected', 'failed', 'degraded', 'cancelled'
    )),
    failure_category  TEXT CHECK (failure_category IS NULL OR failure_category <> ''),
    retryable         INTEGER NOT NULL DEFAULT 0 CHECK (retryable IN (0, 1)),
    retry_at_ms       INTEGER,
    attempt_count     INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    updated_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (account_id, namespace_version, chat_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK (
        (phase IN ('unavailable', 'protected', 'failed', 'degraded'))
        = (failure_category IS NOT NULL)
    ),
    CHECK (retryable = 0 OR phase IN ('unavailable', 'failed', 'degraded')),
    CHECK (retry_at_ms IS NULL OR retryable = 1)
) STRICT, WITHOUT ROWID;

-- Background work considers only runnable rows. Terminal/retry-on-demand
-- phases are absent from this index and therefore cannot become a tight
-- retry loop on large accounts.
CREATE INDEX chat_content_progress_runnable
    ON chat_content_progress
       (account_id, namespace_version, phase, updated_at_ms, chat_id)
    WHERE phase IN ('pending', 'syncing');

-- Every canonical chat has a window row from discovery onward. A windowless,
-- incomplete row is the honest "not anchored yet" state and lets the backlog
-- use one covering partial index rather than a large-account outer join.
INSERT INTO chat_sync_state (
    account_id, namespace_version, chat_id,
    oldest_loaded_message_id, newest_loaded_message_id,
    history_complete, last_sync_at_ms
)
SELECT c.account_id, c.namespace_version, c.chat_id, NULL, NULL, 0, NULL
FROM chats c
LEFT JOIN chat_sync_state s
  ON s.account_id = c.account_id
 AND s.namespace_version = c.namespace_version
 AND s.chat_id = c.chat_id
WHERE s.chat_id IS NULL;

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
 AND s.chat_id = c.chat_id;

CREATE TRIGGER chats_seed_sync_state
AFTER INSERT ON chats
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
    ) VALUES (
        NEW.account_id, NEW.namespace_version, NEW.chat_id,
        CASE WHEN NEW.is_protected = 1 THEN 'protected' ELSE 'pending' END,
        CASE WHEN NEW.is_protected = 1 THEN 'protected-content' ELSE NULL END,
        0, NULL, 0, COALESCE(NEW.last_update_at_ms, 0)
    ) ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;
END;

DROP INDEX chat_sync_state_backlog;
CREATE INDEX chat_sync_state_backlog
    ON chat_sync_state
       (account_id, namespace_version, last_sync_at_ms, chat_id)
    WHERE history_complete = 0;
