-- GramDrive state schema, version 8 (TASK-260721-3e9bi8).
--
-- Canonical story rows remain keyed by (poster_chat_id, story_id). This
-- migration adds the explicit privacy/lifecycle state needed by background
-- discovery, a minimal inaccessible tombstone, and resumable per-chat scan
-- progress. No story text, TDLib file locator, cache path, or bytes are stored
-- by these tables.

ALTER TABLE stories
    ADD COLUMN content_state TEXT NOT NULL DEFAULT 'available'
        CHECK (content_state IN (
            'metadata_pending', 'available', 'protected', 'unsupported',
            'live_unavailable', 'inaccessible'
        ));

ALTER TABLE stories
    ADD COLUMN inaccessible_at_ms INTEGER;

ALTER TABLE story_appearances
    ADD COLUMN profile_scan_generation INTEGER
        CHECK (profile_scan_generation IS NULL OR profile_scan_generation >= 0);

-- v4-v7 had only the forwarding/availability facts. Fail closed while
-- translating them: any non-forwardable or restricted legacy story becomes
-- a redacted protected placeholder; fetchable permitted content remains
-- available; legacy unavailable content keeps its observed facts as
-- inaccessible rather than inventing support or a cause.
UPDATE stories
SET content_state = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN 'protected'
        WHEN availability = 'fetchable' THEN 'available'
        ELSE 'inaccessible'
    END,
    mime_type = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN NULL
        ELSE mime_type
    END,
    exact_size = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN NULL
        ELSE exact_size
    END,
    blob_hash_algo = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN NULL
        ELSE blob_hash_algo
    END,
    blob_hash = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN NULL
        ELSE blob_hash
    END,
    last_verified_at_ms = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN NULL
        ELSE last_verified_at_ms
    END,
    content_version = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted'
            THEN 'story-protected/' || poster_chat_id || '/' || story_id
        ELSE content_version
    END,
    availability = CASE
        WHEN can_be_forwarded = 0 OR availability = 'restricted' THEN 'restricted'
        WHEN availability = 'fetchable' THEN 'fetchable'
        ELSE 'unavailable'
    END;

CREATE TRIGGER stories_protected_insert_guard
BEFORE INSERT ON stories
WHEN NEW.content_state = 'protected' AND (
    NEW.can_be_forwarded <> 0 OR NEW.availability <> 'restricted'
    OR NEW.mime_type IS NOT NULL OR NEW.exact_size IS NOT NULL
    OR NEW.blob_hash IS NOT NULL
)
BEGIN
    SELECT RAISE(ABORT, 'protected story metadata must be redacted');
END;

CREATE TRIGGER stories_protected_update_guard
BEFORE UPDATE OF content_state, can_be_forwarded, availability, mime_type,
                 exact_size, blob_hash ON stories
WHEN NEW.content_state = 'protected' AND (
    NEW.can_be_forwarded <> 0 OR NEW.availability <> 'restricted'
    OR NEW.mime_type IS NOT NULL OR NEW.exact_size IS NOT NULL
    OR NEW.blob_hash IS NOT NULL
)
BEGIN
    SELECT RAISE(ABORT, 'protected story metadata must be redacted');
END;

CREATE TABLE story_tombstones (
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    poster_chat_id      INTEGER NOT NULL,
    story_id            INTEGER NOT NULL,
    observed_at_ms      INTEGER NOT NULL,
    had_profile         INTEGER NOT NULL CHECK (had_profile IN (0, 1)),
    PRIMARY KEY (account_id, namespace_version, poster_chat_id, story_id),
    FOREIGN KEY (account_id, namespace_version, poster_chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE story_sync_progress (
    account_id              INTEGER NOT NULL,
    namespace_version       INTEGER NOT NULL CHECK (namespace_version >= 0),
    chat_id                 INTEGER NOT NULL,
    phase                   TEXT NOT NULL CHECK (phase IN (
        'pending', 'syncing', 'ready', 'unavailable', 'failed', 'cancelled'
    )),
    active_complete         INTEGER NOT NULL DEFAULT 0 CHECK (active_complete IN (0, 1)),
    profile_cursor          INTEGER,
    profile_scan_generation INTEGER NOT NULL DEFAULT 0 CHECK (profile_scan_generation >= 0),
    profile_complete        INTEGER NOT NULL DEFAULT 0 CHECK (profile_complete IN (0, 1)),
    archive_eligibility     TEXT NOT NULL DEFAULT 'unknown' CHECK (archive_eligibility IN (
        'unknown', 'owner', 'manageable', 'ineligible',
        'account_unsupported', 'rights_unavailable'
    )),
    archive_cursor          INTEGER,
    archive_complete        INTEGER NOT NULL DEFAULT 0 CHECK (archive_complete IN (0, 1)),
    pages_committed         INTEGER NOT NULL DEFAULT 0 CHECK (pages_committed >= 0),
    stories_seen            INTEGER NOT NULL DEFAULT 0 CHECK (stories_seen >= 0),
    failure_category        TEXT CHECK (failure_category IS NULL OR failure_category <> ''),
    retryable               INTEGER NOT NULL DEFAULT 0 CHECK (retryable IN (0, 1)),
    attempt_count           INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    updated_at_ms           INTEGER NOT NULL,
    PRIMARY KEY (account_id, namespace_version, chat_id),
    FOREIGN KEY (account_id, namespace_version, chat_id)
        REFERENCES chats (account_id, namespace_version, chat_id) ON DELETE CASCADE,
    CHECK ((phase IN ('unavailable', 'failed')) = (failure_category IS NOT NULL)),
    CHECK (retryable = 0 OR phase IN ('unavailable', 'failed'))
) STRICT, WITHOUT ROWID;

INSERT INTO story_sync_progress (
    account_id, namespace_version, chat_id, phase, updated_at_ms
)
SELECT account_id, namespace_version, chat_id, 'pending',
       COALESCE(last_update_at_ms, 0)
FROM chats
WHERE deleted_at_ms IS NULL;

CREATE INDEX story_sync_progress_runnable
    ON story_sync_progress
       (account_id, namespace_version, updated_at_ms, chat_id)
    WHERE phase IN ('pending', 'syncing');

CREATE TRIGGER chats_seed_story_sync_progress
AFTER INSERT ON chats
BEGIN
    INSERT INTO story_sync_progress (
        account_id, namespace_version, chat_id, phase, updated_at_ms
    ) VALUES (
        NEW.account_id, NEW.namespace_version, NEW.chat_id, 'pending',
        COALESCE(NEW.last_update_at_ms, 0)
    ) ON CONFLICT (account_id, namespace_version, chat_id) DO NOTHING;
END;
