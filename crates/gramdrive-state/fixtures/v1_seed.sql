-- Representative rows of a schema-version-1 database (TASK-260715-18l9xz).
--
-- Applied on top of `src/schema/v1.sql` to build the v1 fixture: the
-- database a v2 migration will be tested against. The AC behind this file is
-- that every migration ships a fixture of the schema it migrates *from* —
-- a migration tested only against a database the current build created has
-- never met the schema it exists for.
--
-- The fixture DDL is v1.sql itself rather than a copy of it here. That is
-- sound only because v1.sql is frozen (a database ever created from it must
-- forever migrate from exactly what it says), and it is what keeps this file
-- reviewable: rows, not 550 lines of duplicated schema. For a future
-- fixture, "the schema at version N" is the baseline plus migrations 2..=N,
-- built by the same frozen scripts that built it in the field.
--
-- What is here and what is not:
--
-- * Canonical source facts — accounts, chats, list membership, the
--   append-only event log and its message projection. This is what data
--   migrations operate on: payload schema families, DOM-021 namespace
--   epochs, POL-3 retention.
-- * No `items` rows. The provider projection is derived state, rebuildable
--   from the tables above (SYNC-071), and its ItemId keys are the
--   gramdrive-model binary codec (DEC-008) — hand-writing those blobs here
--   would put a fake identity encoding in a fixture that claims to be real.
--   A migration that changes projection shape raises a
--   `rebuild_projection` repair marker and lets reconciliation rebuild it;
--   that is the sanctioned path, so the fixture does not pretend otherwise.
--
-- Ids and timestamps are fixed constants — a fixture with a clock in it is a
-- fixture that fails on a Tuesday. The epoch matches
-- gramdrive_testkit::synthetic::SYNTHETIC_EPOCH_MS (2024-01-01T00:00:00Z).

INSERT INTO accounts
    (account_id, source_kind, display_name, auth_state, namespace_version,
     retention_mode, archive_mode, secret_ref, created_at_ms, updated_at_ms)
VALUES
    (7, 'local_tdlib', 'Fixture Account', 'authorized', 1,
     'mirror', 0, 'keychain://fixture', 1704067200000, 1704067200000);

INSERT INTO chats
    (account_id, namespace_version, chat_id, chat_type, title, username,
     is_protected, archive_mode, metadata_version, last_update_at_ms)
VALUES
    (7, 1, 100, 'private', 'Alice', NULL, 0, 0, 'm1', 1704067200000),
    (7, 1, 200, 'group', 'Team', 'teamchat', 0, 0, 'm1', 1704067200000),
    -- Protected content (POL-4) and archived (POL-2): a fixture whose rows
    -- are all defaults proves nothing about the columns that carry policy.
    (7, 1, 300, 'channel', 'News', 'news', 1, 1, 'm2', 1704067200000);

INSERT INTO chat_list_entries
    (account_id, namespace_version, list_kind, folder_id, chat_id, sort_order, pinned)
VALUES
    (7, 1, 'main', 0, 100, 9000, 1),
    (7, 1, 'main', 0, 200, 8000, 0),
    (7, 1, 'archive', 0, 300, 7000, 0),
    (7, 1, 'folder', 42, 200, 6000, 0);

-- event_seq is AUTOINCREMENT; the fixture states it explicitly so the log is
-- byte-identical on every build and `messages.latest_event_seq` below can
-- name its event. Payloads are opaque blobs carrying an explicit schema
-- family (payload_schema) — the shape a lazy payload migration reads.
INSERT INTO message_events
    (event_seq, account_id, namespace_version, chat_id, message_id,
     event_kind, observed_at_ms, payload_schema, payload)
VALUES
    (1,  7, 1, 100, 1, 'observed', 1704067201000, 1, X'A101'),
    (2,  7, 1, 100, 2, 'observed', 1704067202000, 1, X'A102'),
    (3,  7, 1, 100, 3, 'observed', 1704067203000, 1, X'A103'),
    (4,  7, 1, 100, 4, 'observed', 1704067204000, 1, X'A104'),
    (5,  7, 1, 100, 5, 'observed', 1704067205000, 1, X'A105'),
    (6,  7, 1, 200, 1, 'observed', 1704067206000, 1, X'A201'),
    (7,  7, 1, 200, 2, 'observed', 1704067207000, 1, X'A202'),
    (8,  7, 1, 200, 3, 'observed', 1704067208000, 1, X'A203'),
    (9,  7, 1, 200, 4, 'observed', 1704067209000, 1, X'A204'),
    (10, 7, 1, 300, 1, 'observed', 1704067210000, 1, X'A301'),
    (11, 7, 1, 300, 2, 'observed', 1704067211000, 1, X'A302'),
    (12, 7, 1, 300, 3, 'observed', 1704067212000, 1, X'A303'),
    -- An edit: a full new revision of message (100, 2), which is why
    -- `messages` points at this event and not at event 2 (POL-3, DEC-015).
    (13, 7, 1, 100, 2, 'edited', 1704067213000, 1, X'B102'),
    -- An observed deletion: a tombstone, and by CHECK it carries no payload.
    (14, 7, 1, 200, 4, 'deleted', 1704067214000, NULL, NULL),
    -- A Mirror-mode purge leaves the marker with its payload nulled — the
    -- one sanctioned UPDATE the append-only trigger permits. Seeded in its
    -- purged state: a migration must tolerate a payload-less content event.
    (15, 7, 1, 300, 3, 'edited', 1704067215000, NULL, NULL);

INSERT INTO messages
    (account_id, namespace_version, chat_id, message_id, sender_id,
     sent_at_ms, edited_at_ms, is_deleted, latest_event_seq)
VALUES
    (7, 1, 100, 1, 500, 1704067201000, NULL, 0, 1),
    (7, 1, 100, 2, 500, 1704067202000, 1704067213000, 0, 13),
    (7, 1, 100, 3, 501, 1704067203000, NULL, 0, 3),
    (7, 1, 100, 4, 500, 1704067204000, NULL, 0, 4),
    (7, 1, 100, 5, 501, 1704067205000, NULL, 0, 5),
    (7, 1, 200, 1, 600, 1704067206000, NULL, 0, 6),
    (7, 1, 200, 2, 601, 1704067207000, NULL, 0, 7),
    (7, 1, 200, 3, 600, 1704067208000, NULL, 0, 8),
    -- Tombstoned by event 14 but still present: POL-3 keeps the row for sync
    -- correctness even when Mirror mode hides the content.
    (7, 1, 200, 4, 601, 1704067209000, NULL, 1, 14),
    (7, 1, 300, 1, 700, 1704067210000, NULL, 0, 10),
    (7, 1, 300, 2, 700, 1704067211000, NULL, 0, 11),
    (7, 1, 300, 3, 700, 1704067212000, 1704067215000, 0, 15);

INSERT INTO blobs (account_id, hash_algo, hash, size, first_seen_at_ms)
VALUES
    (7, 'sha256', X'0101010101010101010101010101010101010101010101010101010101010101',
     2048, 1704067220000);

INSERT INTO attachments
    (account_id, namespace_version, chat_id, message_id, attachment_index,
     original_name, mime_type, logical_size, content_version,
     telegram_unique_id, telegram_file_id, file_reference, availability,
     can_be_saved, blob_hash_algo, blob_hash, last_verified_at_ms)
VALUES
    -- Downloaded and verified: linked to the blob above.
    (7, 1, 100, 3, 0, 'photo.jpg', 'image/jpeg', 2048, 'cv1',
     'uniq-1', 'file-1', X'DEAD', 'fetchable', 1,
     'sha256', X'0101010101010101010101010101010101010101010101010101010101010101',
     1704067221000),
    -- Known but never fetched: no blob, and POL-4 says there never will be.
    (7, 1, 300, 1, 0, 'secret.pdf', 'application/pdf', 4096, 'cv2',
     'uniq-2', 'file-2', X'BEEF', 'restricted', 0,
     NULL, NULL, NULL);

INSERT INTO change_cursors
    (account_id, namespace_version, stream, cursor_text, updated_at_ms)
VALUES
    (7, 1, 'chat_list', 'cursor-token-42', 1704067230000);

INSERT OR REPLACE INTO chat_sync_state
    (account_id, namespace_version, chat_id, oldest_loaded_message_id,
     newest_loaded_message_id, history_complete, last_sync_at_ms)
VALUES
    -- History walked to the beginning.
    (7, 1, 100, 1, 5, 1, 1704067230000),
    -- Backfill still owes this chat older history (SYNC-021).
    (7, 1, 200, 1, 4, 0, 1704067230000),
    -- Never synced: the window is NULL and the backlog index sorts it first.
    (7, 1, 300, NULL, NULL, 0, NULL);
