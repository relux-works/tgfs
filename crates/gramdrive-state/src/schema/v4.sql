-- GramDrive state schema, version 4 (TASK-260721-1hm7dx).
--
-- Owner-approved date-first content contract. The Rust migration driver
-- applies this DDL and rebuilds legacy projection rows in the same atomic
-- transaction.

ALTER TABLE accounts
    ADD COLUMN display_timezone TEXT NOT NULL DEFAULT 'UTC'
        CHECK (display_timezone <> '');

ALTER TABLE attachments
    ADD COLUMN logical_kind TEXT NOT NULL DEFAULT 'unknown'
        CHECK (logical_kind <> '');
ALTER TABLE attachments
    ADD COLUMN telegram_representation TEXT NOT NULL DEFAULT 'unknown_legacy'
        CHECK (telegram_representation <> '');
ALTER TABLE attachments
    ADD COLUMN fidelity TEXT NOT NULL DEFAULT 'unknown_legacy'
        CHECK (fidelity <> '');
ALTER TABLE attachments
    ADD COLUMN source_name TEXT CHECK (source_name IS NULL OR source_name <> '');
ALTER TABLE attachments
    ADD COLUMN exact_size INTEGER CHECK (
        (exact_size IS NULL OR exact_size >= 0)
        AND (
            telegram_representation NOT IN (
                'message_photo', 'message_video', 'message_animation',
                'message_audio', 'message_voice'
            )
            OR (source_name IS NULL AND fidelity IN ('telegram_variant', 'metadata_only'))
        )
        AND (
            telegram_representation <> 'original_document'
            OR fidelity IN ('original', 'metadata_only')
        )
        AND (
            telegram_representation <> 'unknown_legacy'
            OR fidelity = 'unknown_legacy'
        )
    );

UPDATE attachments
SET source_name = original_name,
    exact_size = logical_size;

CREATE TABLE stories (
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    poster_chat_id      INTEGER NOT NULL,
    story_id            INTEGER NOT NULL,
    source_timestamp_ms INTEGER NOT NULL,
    mime_type           TEXT CHECK (mime_type IS NULL OR mime_type <> ''),
    exact_size          INTEGER CHECK (exact_size IS NULL OR exact_size >= 0),
    content_version     TEXT NOT NULL CHECK (content_version <> ''),
    availability        TEXT NOT NULL
        CHECK (availability IN ('fetchable', 'restricted', 'unavailable')),
    can_be_forwarded    INTEGER NOT NULL CHECK (can_be_forwarded IN (0, 1)),
    blob_hash_algo      TEXT CHECK (blob_hash_algo IN ('sha256')),
    blob_hash           BLOB,
    last_verified_at_ms INTEGER,
    PRIMARY KEY (account_id, namespace_version, poster_chat_id, story_id),
    FOREIGN KEY (account_id, namespace_version, poster_chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    FOREIGN KEY (account_id, blob_hash_algo, blob_hash)
        REFERENCES blobs (account_id, hash_algo, hash),
    CHECK ((blob_hash IS NULL) = (blob_hash_algo IS NULL)),
    CHECK (can_be_forwarded = 1 OR blob_hash IS NULL)
) STRICT, WITHOUT ROWID;

CREATE TABLE story_appearances (
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    poster_chat_id    INTEGER NOT NULL,
    story_id          INTEGER NOT NULL,
    location          TEXT NOT NULL CHECK (location IN ('active', 'month')),
    year              INTEGER,
    month             INTEGER,
    display_name      TEXT NOT NULL CHECK (display_name <> ''),
    posted_at_ms      INTEGER NOT NULL,
    expires_at_ms     INTEGER,
    removed_at_ms     INTEGER,
    PRIMARY KEY (account_id, namespace_version, poster_chat_id, story_id, location),
    FOREIGN KEY (account_id, namespace_version, poster_chat_id, story_id)
        REFERENCES stories (account_id, namespace_version, poster_chat_id, story_id)
        ON DELETE CASCADE,
    CHECK ((location = 'month') = (year IS NOT NULL AND month BETWEEN 1 AND 12)),
    CHECK (location = 'month' OR month IS NULL),
    CHECK (location = 'month' OR year IS NULL)
) STRICT, WITHOUT ROWID;

CREATE INDEX story_appearances_by_month
    ON story_appearances
       (account_id, namespace_version, poster_chat_id, year, month, story_id)
    WHERE location = 'month' AND removed_at_ms IS NULL;

CREATE INDEX story_appearances_active
    ON story_appearances
       (account_id, namespace_version, poster_chat_id, story_id)
    WHERE location = 'active' AND removed_at_ms IS NULL;

DROP INDEX items_children_by_id;
DROP INDEX items_sibling_name;
DROP INDEX items_appearance;
DROP INDEX items_by_scope;

PRAGMA legacy_alter_table = ON;
ALTER TABLE items RENAME TO items_v3;

CREATE TABLE items (
    item_id           BLOB    NOT NULL PRIMARY KEY CHECK (length(item_id) > 0),
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    kind              TEXT    NOT NULL CHECK (kind IN (
        'account', 'chat_list', 'folder_catalog', 'chat',
        'year_dir', 'media_dir',
        'active_stories', 'month_dir', 'canonical_story', 'story_appearance',
        'attachment', 'generated_doc', 'order_doc'
    )),
    parent_item_id    BLOB REFERENCES items (item_id) ON DELETE CASCADE,
    canonical_item_id BLOB CHECK (canonical_item_id IS NULL OR length(canonical_item_id) > 0),
    view_kind         TEXT CHECK (view_kind IN ('main', 'archive', 'folder')),
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

INSERT INTO items (
    item_id, account_id, namespace_version, kind, parent_item_id,
    canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
    is_directory, mime_type, logical_size, metadata_version, content_version,
    availability, created_at_ms, modified_at_ms, deleted_at_ms
)
SELECT
    item_id, account_id, namespace_version, kind, parent_item_id,
    canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
    is_directory, mime_type, logical_size, metadata_version, content_version,
    availability, created_at_ms, modified_at_ms,
    CASE
        WHEN kind IN ('year_dir', 'media_dir')
            THEN COALESCE(deleted_at_ms, unixepoch() * 1000)
        ELSE deleted_at_ms
    END
FROM items_v3;

DROP TABLE items_v3;
PRAGMA legacy_alter_table = OFF;

CREATE INDEX items_children_by_id ON items (parent_item_id, item_id);
CREATE UNIQUE INDEX items_sibling_name
    ON items (parent_item_id, safe_name)
    WHERE parent_item_id IS NOT NULL AND deleted_at_ms IS NULL;
CREATE UNIQUE INDEX items_appearance
    ON items (canonical_item_id, view_kind, COALESCE(view_folder_id, 0))
    WHERE canonical_item_id IS NOT NULL;
CREATE INDEX items_by_scope ON items (account_id, namespace_version);
