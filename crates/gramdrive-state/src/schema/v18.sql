-- GramDrive state schema, version 18 (TASK-260729-376m7o).
--
-- Observability is intentionally aggregate-only. This database must never
-- retain a File Provider identifier, account identity, chat identity, name,
-- or any user content just to explain a callback.

ALTER TABLE items ADD COLUMN tombstone_provenance TEXT;

-- Only reconciliation and retention could create production tombstones
-- before this schema existed. Retention tombstones already carry their
-- fixed pass marker in metadata_version; all other installed tombstones
-- therefore came from projection reconciliation.
UPDATE items
SET tombstone_provenance = CASE
    WHEN metadata_version LIKE 'retention-purge-%' THEN 'retention'
    ELSE 'reconcile'
END
WHERE deleted_at_ms IS NOT NULL;

-- SQLite cannot add a cross-column CHECK to the existing items table. These
-- triggers enforce the same invariant for every future insert/update: live
-- rows have no provenance and every tombstone has one fixed reason code.
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

CREATE TABLE chat_list_commit_audit (
    sequence            INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL,
    list_kind           TEXT NOT NULL,
    folder_id           INTEGER NOT NULL DEFAULT 0,
    before_count        INTEGER NOT NULL CHECK (before_count >= 0),
    after_count         INTEGER NOT NULL CHECK (after_count >= 0),
    is_complete         INTEGER NOT NULL CHECK (is_complete IN (0, 1)),
    committed_at_ms     INTEGER NOT NULL
);

CREATE INDEX chat_list_commit_audit_scope_sequence
    ON chat_list_commit_audit
       (account_id, namespace_version, list_kind, folder_id, sequence DESC);

-- One durable, deliberately identity-free row. The agent owns writes to it;
-- the File Provider only sends bounded telemetry over its control socket.
CREATE TABLE provider_fetch_health (
    singleton                INTEGER PRIMARY KEY CHECK (singleton = 1),
    callback_count           INTEGER NOT NULL DEFAULT 0 CHECK (callback_count >= 0),
    success_count            INTEGER NOT NULL DEFAULT 0 CHECK (success_count >= 0),
    engine_failure_count     INTEGER NOT NULL DEFAULT 0 CHECK (engine_failure_count >= 0),
    provider_mapping_count   INTEGER NOT NULL DEFAULT 0 CHECK (provider_mapping_count >= 0),
    no_such_item_count       INTEGER NOT NULL DEFAULT 0 CHECK (no_such_item_count >= 0),
    retryable_count          INTEGER NOT NULL DEFAULT 0 CHECK (retryable_count >= 0),
    last_updated_at_ms       INTEGER
);

INSERT INTO provider_fetch_health (singleton) VALUES (1);
