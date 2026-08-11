-- GramDrive state schema, version 21 (BUG-260727-2txbfr).
--
-- The Stories view is a first-class provider appearance driven by TDLib
-- storyListMain. It is deliberately distinct from Main, Archive, and custom
-- folders, and it must never enter the ordinary history-seeding trigger.

CREATE TABLE chat_list_entries_v21 (
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    list_kind         TEXT    NOT NULL CHECK (list_kind IN ('main', 'archive', 'stories', 'folder')),
    folder_id         INTEGER NOT NULL DEFAULT 0,
    chat_id           INTEGER NOT NULL,
    sort_order        INTEGER NOT NULL,
    pinned            INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    PRIMARY KEY (account_id, namespace_version, list_kind, folder_id, chat_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK ((list_kind = 'folder') = (folder_id <> 0))
) STRICT, WITHOUT ROWID;

INSERT INTO chat_list_entries_v21
SELECT account_id, namespace_version, list_kind, folder_id, chat_id, sort_order, pinned
FROM chat_list_entries;

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

CREATE TABLE items_v21 (
    item_id           BLOB    NOT NULL PRIMARY KEY CHECK (length(item_id) > 0),
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    kind              TEXT    NOT NULL CHECK (kind IN (
        'account', 'chat_list', 'folder_catalog', 'chat',
        'year_dir', 'media_dir',
        'active_stories', 'month_dir', 'canonical_story', 'story_appearance',
        'attachment', 'generated_doc', 'order_doc'
    )),
    parent_item_id    BLOB REFERENCES items_v21 (item_id) ON DELETE CASCADE,
    canonical_item_id BLOB CHECK (canonical_item_id IS NULL OR length(canonical_item_id) > 0),
    view_kind         TEXT CHECK (view_kind IN ('main', 'archive', 'stories', 'folder')),
    view_folder_id    INTEGER,
    display_name      TEXT    NOT NULL,
    safe_name         TEXT    NOT NULL CHECK (safe_name <> ''),
    is_directory      INTEGER NOT NULL CHECK (is_directory IN (0, 1)),
    mime_type         TEXT CHECK (mime_type IS NULL OR mime_type <> ''),
    logical_size      INTEGER CHECK (logical_size IS NULL OR logical_size >= 0),
    metadata_version  TEXT    NOT NULL CHECK (metadata_version <> ''),
    content_version   TEXT CHECK (content_version IS NULL OR content_version <> ''),
    availability      TEXT    NOT NULL DEFAULT 'fetchable'
        CHECK (availability IN ('fetchable', 'restricted', 'unavailable')),
    created_at_ms     INTEGER,
    modified_at_ms    INTEGER,
    deleted_at_ms     INTEGER,
    aggregate_size    INTEGER,
    tombstone_provenance TEXT,
    CHECK ((parent_item_id IS NULL) = (kind = 'account')),
    CHECK ((canonical_item_id IS NULL) = (view_kind IS NULL)),
    CHECK ((view_folder_id IS NOT NULL) = (view_kind = 'folder')),
    CHECK (is_directory = (kind IN (
        'account', 'chat_list', 'folder_catalog', 'chat',
        'year_dir', 'media_dir', 'active_stories', 'month_dir'
    ))),
    CHECK (is_directory = 0 OR
           (mime_type IS NULL AND logical_size IS NULL AND content_version IS NULL)),
    CHECK (kind NOT IN ('year_dir', 'media_dir') OR deleted_at_ms IS NOT NULL)
) STRICT;

INSERT INTO items_v21 (
    item_id, account_id, namespace_version, kind, parent_item_id,
    canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
    is_directory, mime_type, logical_size, metadata_version, content_version,
    availability, created_at_ms, modified_at_ms, deleted_at_ms, aggregate_size,
    tombstone_provenance
)
SELECT
    item_id, account_id, namespace_version, kind, parent_item_id,
    canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
    is_directory, mime_type, logical_size, metadata_version, content_version,
    availability, created_at_ms, modified_at_ms, deleted_at_ms, aggregate_size,
    tombstone_provenance
FROM items;

DROP TABLE items;
ALTER TABLE items_v21 RENAME TO items;

CREATE INDEX items_children_by_id ON items (parent_item_id, item_id);
CREATE UNIQUE INDEX items_sibling_name
    ON items (parent_item_id, safe_name)
    WHERE parent_item_id IS NOT NULL AND deleted_at_ms IS NULL;
CREATE INDEX items_by_scope ON items (account_id, namespace_version);
CREATE UNIQUE INDEX items_appearance
    ON items (canonical_item_id, view_kind, COALESCE(view_folder_id, 0))
    WHERE canonical_item_id IS NOT NULL AND kind <> 'story_appearance';
CREATE INDEX items_by_canonical_item
    ON items (canonical_item_id, item_id)
    WHERE canonical_item_id IS NOT NULL;
CREATE INDEX items_live_generated_docs_by_parent
    ON items (parent_item_id, item_id)
    WHERE kind = 'generated_doc' AND deleted_at_ms IS NULL;

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
