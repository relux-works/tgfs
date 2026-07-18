-- GramDrive state schema, version 1 (TASK-260715-1ceq7h).
--
-- Applied atomically by gramdrive-state when it opens a database whose
-- user_version is 0; the runner sets user_version to 1 in the same
-- transaction. Later schema versions are separate migration scripts
-- (TASK-260715-18l9xz), never edits to this file: a database that was ever
-- created from this script must forever migrate from exactly what this
-- script says.
--
-- Design ground rules, enforced here rather than promised in Rust:
--
-- * Every table is STRICT. A TEXT that silently stores an integer is the
--   kind of corruption that surfaces months later in a provider callback.
-- * Item identities are the canonical binary ItemId encoding from
--   gramdrive-model (DEC-008): opaque BLOBs here, decodable and versioned
--   there. The database never interprets them; it only guarantees they are
--   present, non-empty, and consistently linked.
-- * Source facts (chats, messages, attachments) are keyed by their Telegram
--   coordinates scoped to (account_id, namespace_version) — DOM-021. A
--   namespace bump retires a whole epoch's rows without renumbering the
--   account.
-- * The append-only message event log is the canonical store (POL-3,
--   DEC-015); `messages` is a mutable projection over it. Append-only is a
--   trigger, not a convention.
-- * Paths are never foreign keys (DOM-005). The tree lives in
--   parent_item_id links; names are payload.
-- * Timestamps are INTEGER milliseconds since the Unix epoch, always
--   source-explicit (SYNC-073). Columns end in _at_ms.

-- ---------------------------------------------------------------------------
-- accounts — one row per configured source identity (domain-model § Account).
--
-- namespace_version is the *current* epoch (DOM-021); rows of retired epochs
-- in the scoped tables below stay until reconciliation sweeps them.
-- retention_mode is the per-account POL-3 selection. secret_ref is a
-- reference into platform secure storage — never key material (SEC).
-- ---------------------------------------------------------------------------
CREATE TABLE accounts (
    account_id        INTEGER NOT NULL PRIMARY KEY,
    source_kind       TEXT    NOT NULL CHECK (source_kind IN ('local_tdlib', 'remote_http')),
    display_name      TEXT    NOT NULL,
    auth_state        TEXT    NOT NULL CHECK (auth_state <> ''),
    namespace_version INTEGER NOT NULL DEFAULT 0 CHECK (namespace_version >= 0),
    retention_mode    TEXT    NOT NULL DEFAULT 'mirror' CHECK (retention_mode IN ('mirror', 'audit')),
    archive_mode      INTEGER NOT NULL DEFAULT 0 CHECK (archive_mode IN (0, 1)),
    secret_ref        TEXT             CHECK (secret_ref IS NULL OR secret_ref <> ''),
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------
-- chats — canonical Telegram chat metadata (domain-model § Chat), one row per
-- chat per namespace epoch. Independent of every view: list membership and
-- order live in chat_list_entries, presentation in items (SYNC-026).
--
-- archive_mode is the per-chat POL-2 toggle; is_protected mirrors Telegram's
-- protected-content flag (POL-4). left_at_ms / deleted_at_ms are POL-3
-- tombstone markers — rows are removed only by retention policy, not by
-- observation.
-- ---------------------------------------------------------------------------
CREATE TABLE chats (
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id           INTEGER NOT NULL,
    chat_type         TEXT    NOT NULL CHECK (chat_type IN ('private', 'group', 'supergroup', 'channel')),
    title             TEXT    NOT NULL,
    username          TEXT             CHECK (username IS NULL OR username <> ''),
    is_protected      INTEGER NOT NULL DEFAULT 0 CHECK (is_protected IN (0, 1)),
    archive_mode      INTEGER NOT NULL DEFAULT 0 CHECK (archive_mode IN (0, 1)),
    metadata_version  TEXT    NOT NULL CHECK (metadata_version <> ''),
    left_at_ms        INTEGER,
    deleted_at_ms     INTEGER,
    last_update_at_ms INTEGER,
    PRIMARY KEY (account_id, namespace_version, chat_id)
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- chat_list_entries — membership and exact order of a chat in one chat list
-- (Main, Archive, or a custom folder): the source facts POL-1's order.json
-- and the app's canonical order are regenerated from (DEC-013).
--
-- folder_id 0 is the sentinel for the built-in lists, so the composite key
-- stays NOT NULL (WITHOUT ROWID keys cannot carry NULL); the CHECK makes the
-- sentinel unambiguous. sort_order is Telegram's opaque i64 position —
-- larger sorts first; pinned chats sort before everything (POL-1).
-- ---------------------------------------------------------------------------
CREATE TABLE chat_list_entries (
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    list_kind         TEXT    NOT NULL CHECK (list_kind IN ('main', 'archive', 'folder')),
    folder_id         INTEGER NOT NULL DEFAULT 0,
    chat_id           INTEGER NOT NULL,
    sort_order        INTEGER NOT NULL,
    pinned            INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    PRIMARY KEY (account_id, namespace_version, list_kind, folder_id, chat_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK ((list_kind = 'folder') = (folder_id <> 0))
) STRICT, WITHOUT ROWID;

-- order.json regeneration and app-UI order: one list, exact order, no scan
-- (POL-1). Pinned first, then Telegram order descending.
CREATE INDEX chat_list_entries_order
    ON chat_list_entries (account_id, namespace_version, list_kind, folder_id, pinned DESC, sort_order DESC);

-- FK support: deleting or re-listing one chat must not scan every list.
CREATE INDEX chat_list_entries_by_chat
    ON chat_list_entries (account_id, namespace_version, chat_id);

-- ---------------------------------------------------------------------------
-- message_events — the append-only canonical message log (POL-3, DEC-015).
--
-- Every observation is one appended row: first sight ('observed'), an edit
-- ('edited', a full new revision), or an observed deletion ('deleted', a
-- tombstone that never carries content). History that was never observed is
-- never implied (domain-model § Message record).
--
-- event_seq is AUTOINCREMENT on purpose: sequence numbers are watermarks
-- (render_state.input_watermark_seq) and must never be reused, even after a
-- policy purge deletes rows.
--
-- Append-only is enforced by trigger below. The single sanctioned UPDATE is
-- the Mirror-mode content purge: payload and payload_schema go to NULL
-- together, leaving the minimal marker (ids, kind, timestamp) POL-3 keeps
-- for sync correctness. Row deletion remains possible for the Audit-mode
-- explicit purge tool and account/epoch removal.
--
-- payload is the normalized message record (schema family in
-- payload_schema) — raw enough for lossless migration, never interpreted by
-- SQL. 'deleted' events carry no payload by CHECK.
-- ---------------------------------------------------------------------------
CREATE TABLE message_events (
    event_seq         INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id           INTEGER NOT NULL,
    message_id        INTEGER NOT NULL,
    event_kind        TEXT    NOT NULL CHECK (event_kind IN ('observed', 'edited', 'deleted')),
    observed_at_ms    INTEGER NOT NULL,
    payload_schema    INTEGER          CHECK (payload_schema IS NULL OR payload_schema >= 0),
    payload           BLOB,
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK ((payload IS NULL) = (payload_schema IS NULL)),
    CHECK (event_kind <> 'deleted' OR payload IS NULL)
) STRICT;

CREATE TRIGGER message_events_append_only
BEFORE UPDATE ON message_events
FOR EACH ROW
WHEN NOT (
    NEW.event_seq = OLD.event_seq
    AND NEW.account_id = OLD.account_id
    AND NEW.namespace_version = OLD.namespace_version
    AND NEW.chat_id = OLD.chat_id
    AND NEW.message_id = OLD.message_id
    AND NEW.event_kind = OLD.event_kind
    AND NEW.observed_at_ms = OLD.observed_at_ms
    AND ((NEW.payload IS OLD.payload AND NEW.payload_schema IS OLD.payload_schema)
         OR (NEW.payload IS NULL AND NEW.payload_schema IS NULL))
)
BEGIN
    SELECT RAISE(ABORT, 'message_events is append-only (POL-3): only a payload purge may update a row');
END;

-- Render catch-up: "every event of this chat after watermark W, in order"
-- (SYNC-022, SYNC-024).
CREATE INDEX message_events_by_chat
    ON message_events (account_id, namespace_version, chat_id, event_seq);

-- Audit-mode revision history of one message, in observation order (POL-3).
CREATE INDEX message_events_by_message
    ON message_events (account_id, namespace_version, chat_id, message_id, event_seq);

-- ---------------------------------------------------------------------------
-- messages — current observed state, one row per message: the projection the
-- tree and renderers read without replaying the log. latest_event_seq points
-- at the event that produced this state; the FK (deliberately NOT cascading)
-- refuses to purge an event that is still someone's current state.
--
-- is_deleted is the POL-3 tombstone bit: Mirror mode hides and purges
-- content but the row may stay for sync correctness; Audit mode keeps it
-- visible. sent_at_ms orders rendering; message_id orders history traversal
-- (SYNC-021 idempotence by Telegram identity).
-- ---------------------------------------------------------------------------
CREATE TABLE messages (
    account_id        INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id           INTEGER NOT NULL,
    message_id        INTEGER NOT NULL,
    sender_id         INTEGER,
    sent_at_ms        INTEGER NOT NULL,
    edited_at_ms      INTEGER          CHECK (edited_at_ms IS NULL OR edited_at_ms >= sent_at_ms),
    is_deleted        INTEGER NOT NULL DEFAULT 0 CHECK (is_deleted IN (0, 1)),
    latest_event_seq  INTEGER NOT NULL REFERENCES message_events (event_seq),
    PRIMARY KEY (account_id, namespace_version, chat_id, message_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- Month/year partition rendering reads a chat's messages by time window
-- (SYNC-031); the PK already serves id-ordered traversal (SYNC-021).
CREATE INDEX messages_by_time
    ON messages (account_id, namespace_version, chat_id, sent_at_ms);

-- FK support: a POL-3 purge deletes event rows; finding the messages that
-- still reference one must not scan the table.
CREATE INDEX messages_by_latest_event
    ON messages (latest_event_seq);

-- ---------------------------------------------------------------------------
-- blobs — fully downloaded, hash-verified content (domain-model § Blob).
-- Content-addressed within one account (BlobKey): the same bytes in two
-- accounts are two rows, so one account's holdings are unobservable from
-- another's. Partial downloads are transfers, never blobs.
-- ---------------------------------------------------------------------------
CREATE TABLE blobs (
    account_id       INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    hash_algo        TEXT    NOT NULL CHECK (hash_algo IN ('sha256')),
    hash             BLOB    NOT NULL CHECK (length(hash) = 32),
    size             INTEGER NOT NULL CHECK (size >= 0),
    first_seen_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, hash_algo, hash)
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- attachments — one downloadable object per (message, ordinal)
-- (domain-model § Attachment; DOM-021). telegram_file_id / file_reference
-- are refreshable locators, never identity (DOM-007, SYNC-045): refreshing
-- them touches nothing else. blob_hash links to verified bytes once a
-- download completes; blob identity never replaces attachment identity.
-- availability carries POL-4: 'restricted' and 'view_once' bytes are never
-- fetched.
-- ---------------------------------------------------------------------------
CREATE TABLE attachments (
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id             INTEGER NOT NULL,
    message_id          INTEGER NOT NULL,
    attachment_index    INTEGER NOT NULL CHECK (attachment_index >= 0),
    original_name       TEXT             CHECK (original_name IS NULL OR original_name <> ''),
    mime_type           TEXT             CHECK (mime_type IS NULL OR mime_type <> ''),
    logical_size        INTEGER          CHECK (logical_size IS NULL OR logical_size >= 0),
    content_version     TEXT    NOT NULL CHECK (content_version <> ''),
    telegram_unique_id  TEXT             CHECK (telegram_unique_id IS NULL OR telegram_unique_id <> ''),
    telegram_file_id    TEXT             CHECK (telegram_file_id IS NULL OR telegram_file_id <> ''),
    file_reference      BLOB,
    availability        TEXT    NOT NULL DEFAULT 'fetchable'
                                         CHECK (availability IN ('fetchable', 'restricted', 'unavailable', 'view_once')),
    can_be_saved        INTEGER NOT NULL DEFAULT 1 CHECK (can_be_saved IN (0, 1)),
    blob_hash_algo      TEXT             CHECK (blob_hash_algo IN ('sha256')),
    blob_hash           BLOB,
    last_verified_at_ms INTEGER,
    PRIMARY KEY (account_id, namespace_version, chat_id, message_id, attachment_index),
    FOREIGN KEY (account_id, namespace_version, chat_id, message_id)
        REFERENCES messages (account_id, namespace_version, chat_id, message_id) ON DELETE CASCADE,
    FOREIGN KEY (account_id, blob_hash_algo, blob_hash)
        REFERENCES blobs (account_id, hash_algo, hash),
    CHECK ((blob_hash IS NULL) = (blob_hash_algo IS NULL))
) STRICT, WITHOUT ROWID;

-- Blob back-references: eviction and dedup ask "who still needs these
-- bytes" (SYNC-052); partial — most attachments have no blob yet.
CREATE INDEX attachments_by_blob
    ON attachments (account_id, blob_hash_algo, blob_hash)
    WHERE blob_hash IS NOT NULL;

-- ---------------------------------------------------------------------------
-- items — the provider projection: every node a native provider can see,
-- keyed by its stable ItemId (DEC-008, DOM-001/DOM-024). Rebuildable from
-- the canonical tables (SYNC-071); persisted so lookup and enumeration are
-- index reads, not tree derivations, and so provider-side state (transfers,
-- cache, pins, render state) has one durable key to hang off.
--
-- Two flavors in one table (DOM-002, DOM-022):
--   * canonical structural nodes — the account root, the chat-list roots,
--     the folder catalog: canonical_item_id and view_kind are NULL;
--   * appearance nodes — everything below a view root: the appearance
--     ItemId is the key, canonical_item_id holds the wrapped canonical
--     ItemId, view_kind/view_folder_id name the view. The same canonical
--     chat in Main and in a folder is two rows over one chats record —
--     never two chats records (SYNC-010).
--
-- canonical_item_id is deliberately not a foreign key: the canonical side
-- of a chat/message/attachment/doc lives in its own typed table, reached by
-- decoding the ItemId — one identity namespace, many canonical stores
-- (DOM-024). The tree structure itself *is* enforced: parent_item_id is a
-- real self-FK, NULL exactly at the account root.
--
-- kind mirrors the CanonicalKey vocabulary of gramdrive-model, minus
-- 'message' and 'blob': v1 surfaces neither as a provider node (messages
-- render into generated docs; blobs back attachments).
--
-- deleted_at_ms tombstones a node per POL-3 without breaking the sibling
-- uniqueness of live names (the partial unique index skips tombstones).
-- ---------------------------------------------------------------------------
CREATE TABLE items (
    item_id           BLOB    NOT NULL PRIMARY KEY CHECK (length(item_id) > 0),
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    kind              TEXT    NOT NULL CHECK (kind IN ('account', 'chat_list', 'folder_catalog', 'chat',
                                                      'year_dir', 'media_dir', 'attachment',
                                                      'generated_doc', 'order_doc')),
    parent_item_id    BLOB             REFERENCES items (item_id) ON DELETE CASCADE,
    canonical_item_id BLOB             CHECK (canonical_item_id IS NULL OR length(canonical_item_id) > 0),
    view_kind         TEXT             CHECK (view_kind IN ('main', 'archive', 'folder')),
    view_folder_id    INTEGER,
    display_name      TEXT    NOT NULL,
    safe_name         TEXT    NOT NULL CHECK (safe_name <> ''),
    is_directory      INTEGER NOT NULL CHECK (is_directory IN (0, 1)),
    mime_type         TEXT             CHECK (mime_type IS NULL OR mime_type <> ''),
    logical_size      INTEGER          CHECK (logical_size IS NULL OR logical_size >= 0),
    metadata_version  TEXT    NOT NULL CHECK (metadata_version <> ''),
    content_version   TEXT             CHECK (content_version IS NULL OR content_version <> ''),
    availability      TEXT    NOT NULL DEFAULT 'fetchable'
                                       CHECK (availability IN ('fetchable', 'restricted', 'unavailable')),
    created_at_ms     INTEGER,
    modified_at_ms    INTEGER,
    deleted_at_ms     INTEGER,
    -- The account root, and only the account root, has no parent.
    CHECK ((parent_item_id IS NULL) = (kind = 'account')),
    -- Appearance columns come as a unit.
    CHECK ((canonical_item_id IS NULL) = (view_kind IS NULL)),
    CHECK ((view_folder_id IS NOT NULL) = (view_kind IS NOT NULL AND view_kind = 'folder')),
    -- Directory-ness is a function of kind, and directories carry no
    -- content facts.
    CHECK (is_directory = (kind IN ('account', 'chat_list', 'folder_catalog', 'chat', 'year_dir', 'media_dir'))),
    CHECK (is_directory = 0 OR (mime_type IS NULL AND logical_size IS NULL AND content_version IS NULL))
) STRICT;

-- Paged enumeration (SYNC-003): children of one parent in stable ItemId
-- order, page-anchored by the last returned id.
CREATE INDEX items_children_by_id
    ON items (parent_item_id, item_id);

-- Two live siblings may never share a filesystem name (SYNC-012/SYNC-013
-- output discipline). Tombstoned rows keep their name without blocking a
-- live successor.
CREATE UNIQUE INDEX items_sibling_name
    ON items (parent_item_id, safe_name)
    WHERE parent_item_id IS NOT NULL AND deleted_at_ms IS NULL;

-- One appearance per (canonical item, view) — DOM-022. COALESCE folds the
-- NULL folder id of built-in views to the 0 sentinel; without it SQLite
-- would treat every NULL as distinct and the uniqueness would be fiction.
CREATE UNIQUE INDEX items_appearance
    ON items (canonical_item_id, view_kind, COALESCE(view_folder_id, 0))
    WHERE canonical_item_id IS NOT NULL;

-- Epoch retirement and account cascade support (DOM-021).
CREATE INDEX items_by_scope
    ON items (account_id, namespace_version);

-- ---------------------------------------------------------------------------
-- transfers — durable hydration operations (domain-model § Transfer;
-- SYNC-040..046). One row per attempt-in-progress or terminal record;
-- content_version pins the version the bytes are valid for (SYNC-042 —
-- fetched for A, never published as B). requested/completed ranges are JSON
-- arrays of [start, end) pairs — validated as JSON here, interpreted by the
-- engine. failure_category is the SYNC-044 taxonomy: the gramdrive-source
-- error categories plus the two local ones (disk_full, integrity).
-- ---------------------------------------------------------------------------
CREATE TABLE transfers (
    transfer_id      INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    item_id          BLOB    NOT NULL REFERENCES items (item_id) ON DELETE CASCADE,
    content_version  TEXT    NOT NULL CHECK (content_version <> ''),
    state            TEXT    NOT NULL CHECK (state IN ('queued', 'running', 'suspended',
                                                      'done', 'failed', 'cancelled')),
    priority         INTEGER NOT NULL DEFAULT 0,
    requested_ranges TEXT    NOT NULL CHECK (json_valid(requested_ranges)),
    completed_ranges TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(completed_ranges)),
    temp_ref         TEXT             CHECK (temp_ref IS NULL OR temp_ref <> ''),
    retry_count      INTEGER NOT NULL DEFAULT 0 CHECK (retry_count >= 0),
    next_retry_at_ms INTEGER,
    failure_category TEXT             CHECK (failure_category IN ('invalid_request', 'not_found',
                                                                 'auth_required', 'rate_limited',
                                                                 'restricted', 'stale_reference',
                                                                 'version_conflict', 'unavailable',
                                                                 'cancelled', 'internal',
                                                                 'disk_full', 'integrity')),
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
    created_at_ms    INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL,
    -- A failure without a category would be unclassifiable for retry policy
    -- (SYNC-044); a terminal 'done' must not carry one.
    CHECK (state <> 'failed' OR failure_category IS NOT NULL),
    CHECK (state <> 'done' OR failure_category IS NULL)
) STRICT;

-- Scheduler head: next live transfer by priority, no scan over terminal
-- rows (they dominate over time). The predicate is OR-form, not IN:
-- SQLite's partial-index prover accepts "state = 'queued'" against an OR
-- of equalities but not against an IN list, and an index a query cannot
-- prove is an index that does not exist.
CREATE INDEX transfers_queue
    ON transfers (state, priority DESC, transfer_id)
    WHERE state = 'queued' OR state = 'running' OR state = 'suspended';

-- Coalescing (SYNC-046) and FK support: live work for one item/version.
CREATE INDEX transfers_by_item
    ON transfers (item_id, content_version);

-- ---------------------------------------------------------------------------
-- cache_entries — materialized bytes per provider item (domain-model
-- § Cache entry; POL-2). kind separates the SYNC-050 accounting categories
-- that live here (partial transfer bytes are accounted via transfers;
-- required metadata is the database itself). pinned mirrors durable pin
-- intent onto the materialized row so the eviction scan needs no join;
-- `pins` below is the intent that exists before and independent of
-- materialization. verification gates eviction: only verified content is
-- LRU-evictable (SYNC-052); corrupt entries await repair, unverified ones
-- await hashing. materialization_ref is the platform's opaque handle to the
-- on-disk form (APFS clone id, provider bookmark) — never interpreted here.
-- ---------------------------------------------------------------------------
CREATE TABLE cache_entries (
    item_id             BLOB    NOT NULL PRIMARY KEY REFERENCES items (item_id) ON DELETE CASCADE,
    account_id          INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    content_version     TEXT    NOT NULL CHECK (content_version <> ''),
    kind                TEXT    NOT NULL CHECK (kind IN ('blob', 'generated_doc', 'thumbnail')),
    size                INTEGER NOT NULL CHECK (size >= 0),
    blob_hash_algo      TEXT             CHECK (blob_hash_algo IN ('sha256')),
    blob_hash           BLOB,
    verification        TEXT    NOT NULL DEFAULT 'unverified'
                                         CHECK (verification IN ('unverified', 'verified', 'corrupt')),
    pinned              INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    pin_origin          TEXT             CHECK (pin_origin IN ('user', 'archive_mode')),
    last_access_at_ms   INTEGER NOT NULL,
    materialized_at_ms  INTEGER NOT NULL,
    materialization_ref TEXT             CHECK (materialization_ref IS NULL OR materialization_ref <> ''),
    FOREIGN KEY (account_id, blob_hash_algo, blob_hash)
        REFERENCES blobs (account_id, hash_algo, hash),
    CHECK ((pinned = 1) = (pin_origin IS NOT NULL)),
    CHECK ((blob_hash IS NULL) = (blob_hash_algo IS NULL))
) STRICT, WITHOUT ROWID;

-- The LRU eviction scan (POL-2, SYNC-051/052): eligible rows only, oldest
-- access first. Partial, so pinned and unverified content is not even in
-- the index.
CREATE INDEX cache_entries_eviction
    ON cache_entries (last_access_at_ms)
    WHERE pinned = 0 AND verification = 'verified';

-- Quota accounting by account and category (SYNC-050); covering, so the
-- sum never touches the table.
CREATE INDEX cache_entries_accounting
    ON cache_entries (account_id, kind, size);

-- ---------------------------------------------------------------------------
-- pins — durable "available offline" intent per provider item (POL-2).
-- Exists before hydration and survives eviction of everything else; the
-- engine folds it into cache_entries.pinned on materialization and expands
-- directory pins (a pinned chat pins its subtree) itself. origin separates
-- an explicit user pin from Archive-Mode coverage so turning Archive Mode
-- off releases exactly its own pins.
-- ---------------------------------------------------------------------------
CREATE TABLE pins (
    item_id       BLOB    NOT NULL PRIMARY KEY REFERENCES items (item_id) ON DELETE CASCADE,
    origin        TEXT    NOT NULL CHECK (origin IN ('user', 'archive_mode')),
    created_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- change_cursors — one durable feed position per (account, stream)
-- (DOM-004, SYNC-004, SYNC-022). cursor_text is the versioned ChangeCursor
-- encoding from gramdrive-model, which carries its own scope; the
-- namespace_version column repeats the epoch so retirement sweeps are SQL,
-- but scope *verification* is ChangeCursor::require_scope at load — the
-- database stores cursors, it does not trust them.
-- ---------------------------------------------------------------------------
CREATE TABLE change_cursors (
    account_id        INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version INTEGER NOT NULL CHECK (namespace_version >= 0),
    stream            TEXT    NOT NULL CHECK (stream <> ''),
    cursor_text       TEXT    NOT NULL CHECK (cursor_text <> ''),
    updated_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (account_id, stream)
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- chat_sync_state — resumable per-chat history traversal (SYNC-021): the
-- contiguous [oldest, newest] window of message ids already normalized, and
-- whether backfill reached the beginning of history. The bounds move only
-- with normalized state in the same transaction (SYNC-022).
-- ---------------------------------------------------------------------------
CREATE TABLE chat_sync_state (
    account_id               INTEGER NOT NULL,
    namespace_version        INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id                  INTEGER NOT NULL,
    oldest_loaded_message_id INTEGER,
    newest_loaded_message_id INTEGER,
    history_complete         INTEGER NOT NULL DEFAULT 0 CHECK (history_complete IN (0, 1)),
    last_sync_at_ms          INTEGER,
    PRIMARY KEY (account_id, namespace_version, chat_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK ((oldest_loaded_message_id IS NULL) = (newest_loaded_message_id IS NULL)),
    CHECK (oldest_loaded_message_id IS NULL OR oldest_loaded_message_id <= newest_loaded_message_id)
) STRICT, WITHOUT ROWID;

-- Backfill scheduling: incomplete chats of one scope, least-recently
-- synced first (NULL last_sync_at_ms sorts first — never-synced chats lead).
CREATE INDEX chat_sync_state_backlog
    ON chat_sync_state (account_id, namespace_version, last_sync_at_ms)
    WHERE history_complete = 0;

-- ---------------------------------------------------------------------------
-- backfill_control — durable per-scope backfill scheduler state
-- (TASK-260715-mua1ng; POL-2/DEC-014, NFR-031, SYNC-070, SEC-031, NFR-033).
-- The engine's metadata-first backfill scheduler keeps no state in memory: the
-- pause a user set and the flood-wait a Telegram 429 mandated must both survive
-- a process restart (NFR-031, SYNC-070), or a crash would resume paused work or
-- re-hammer an account still under a flood wait (a ban risk; NFR-033: a flood
-- wait is never a tight retry loop). One row per scope.
--   paused              — user pause switch (task AC user-pausable;
--                         SYNC-043/SYNC-005 durable resumable state).
--   next_request_at_ms  — account-global request spacer: the earliest wall
--                         clock at which the next provider request may issue
--                         (SEC-031 request-concurrency bound over time).
--   flood_wait_until_ms — a honored Telegram flood-wait deadline; no request
--                         issues before it (NFR-033: flood waits are never a
--                         tight retry loop). Distinct from the spacer so a
--                         flood wait is observable on its own.
-- The scheduler's own flood-wait attempt budget uses the source machine's
-- per-request attempt count (passed in), so no durable fault counter lives
-- here. Namespace_version is carried (not just account_id) because a bump
-- retires the backlog it paces; the row is scope-keyed like chat_sync_state.
-- ---------------------------------------------------------------------------
CREATE TABLE backfill_control (
    account_id          INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    paused              INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    next_request_at_ms  INTEGER,
    flood_wait_until_ms INTEGER,
    updated_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (account_id, namespace_version)
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- render_state — per generated document (domain-model § Generated document;
-- SYNC-024, SYNC-030..033): which renderer/schema produced the published
-- bytes, from inputs up to which event watermark, and whether the document
-- needs re-rendering. input_watermark_seq is a message_events.event_seq:
-- "this document reflects every event at or below". content_version is what
-- providers see (DOM-006 composite); hash and size exist once materialized.
-- ---------------------------------------------------------------------------
CREATE TABLE render_state (
    item_id             BLOB    NOT NULL PRIMARY KEY REFERENCES items (item_id) ON DELETE CASCADE,
    renderer_version    INTEGER NOT NULL CHECK (renderer_version >= 0),
    schema_version      INTEGER NOT NULL CHECK (schema_version >= 0),
    input_watermark_seq INTEGER NOT NULL DEFAULT 0 CHECK (input_watermark_seq >= 0),
    content_version     TEXT             CHECK (content_version IS NULL OR content_version <> ''),
    content_hash_algo   TEXT             CHECK (content_hash_algo IN ('sha256')),
    content_hash        BLOB,
    logical_size        INTEGER          CHECK (logical_size IS NULL OR logical_size >= 0),
    dirty               INTEGER NOT NULL DEFAULT 1 CHECK (dirty IN (0, 1)),
    rendered_at_ms      INTEGER,
    CHECK ((content_hash IS NULL) = (content_hash_algo IS NULL))
) STRICT, WITHOUT ROWID;

-- The re-render worklist (SYNC-024): dirty documents only, covering.
CREATE INDEX render_state_dirty
    ON render_state (item_id)
    WHERE dirty = 1;

-- ---------------------------------------------------------------------------
-- schema_history — one row per schema version ever applied to this file.
-- user_version answers "what is current" in one pragma read; this table
-- answers "how did we get here" for the migration runner (SYNC-072,
-- NFR-041) and for diagnostics.
-- ---------------------------------------------------------------------------
CREATE TABLE schema_history (
    version       INTEGER NOT NULL PRIMARY KEY CHECK (version > 0),
    applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms >= 0)
) STRICT;

INSERT INTO schema_history (version, applied_at_ms)
VALUES (1, unixepoch() * 1000);
