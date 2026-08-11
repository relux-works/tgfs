-- Representative schema-version-2 fixture for the v3 metadata migration.
-- The schema is built by applying frozen v1 + v2 before this seed. These
-- source rows prove v3 leaves existing canonical chats and memberships intact.
INSERT INTO accounts
    (account_id, source_kind, display_name, auth_state, namespace_version,
     retention_mode, archive_mode, created_at_ms, updated_at_ms)
VALUES (7, 'local_tdlib', 'Fixture Account', 'authorized', 1,
        'mirror', 0, 1704067200000, 1704067200000);

INSERT INTO chats
    (account_id, namespace_version, chat_id, chat_type, title, is_protected,
     archive_mode, metadata_version, last_update_at_ms)
VALUES (7, 1, 100, 'private', 'Fixture Chat', 0, 0, 'm1', 1704067200000);

INSERT INTO chat_list_entries
    (account_id, namespace_version, list_kind, folder_id, chat_id, sort_order, pinned)
VALUES (7, 1, 'main', 0, 100, 9000, 0);
