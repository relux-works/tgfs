-- GramDrive state schema, version 11 (TASK-260721-2tamdj).
--
-- Audit-to-Mirror removes database ownership atomically. Physical cache files
-- live outside SQLite, so their opaque handles are persisted here before the
-- cache rows are deleted. The coordinator retries each row until the file is
-- absent, then acknowledges it. Account scoping prevents one account's purge
-- status from exposing another account's retained holdings.

CREATE TABLE retention_purge_queue (
    account_id          INTEGER NOT NULL
        REFERENCES accounts (account_id) ON DELETE CASCADE,
    materialization_ref TEXT NOT NULL CHECK (materialization_ref <> ''),
    queued_at_ms        INTEGER NOT NULL,
    PRIMARY KEY (account_id, materialization_ref)
) STRICT, WITHOUT ROWID;

CREATE INDEX retention_purge_queue_by_time
    ON retention_purge_queue (account_id, queued_at_ms, materialization_ref);
