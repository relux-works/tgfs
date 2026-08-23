-- GramDrive state schema, version 21 (BUG-260727-2txbfr;
-- BUG-260823-kd815p).
--
-- Large installed namespaces copy into these shadow tables in bounded
-- transactions. The old v20 tables remain authoritative until the final
-- chunk atomically swaps both shadows and stamps user_version 21.

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
