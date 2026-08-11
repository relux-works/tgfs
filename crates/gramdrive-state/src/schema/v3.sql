-- GramDrive state schema, version 3 (BUG-260721-3tilmj).
--
-- Metadata-only Telegram namespace state: the ordered user folder catalog
-- and the atomic resume token for the bounded chat-list snapshot. No message
-- history, media bytes, credentials, or tokens from Telegram are stored here.

CREATE TABLE chat_folders (
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    folder_id          INTEGER NOT NULL CHECK (folder_id <> 0),
    title              TEXT    NOT NULL,
    position           INTEGER NOT NULL CHECK (position >= 0),
    PRIMARY KEY (account_id, namespace_version, folder_id),
    UNIQUE (account_id, namespace_version, position)
) STRICT;

CREATE INDEX chat_folders_in_catalog_order
    ON chat_folders (account_id, namespace_version, position, folder_id);

CREATE TABLE namespace_bootstrap (
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    resume_token      BLOB    NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (account_id, namespace_version)
) STRICT;
