-- GramDrive state schema, version 9 (TASK-260721-3e9bi8).
--
-- Save-permitted stories retain byte-free TDLib locator facts independently
-- from the one canonical blob link. Profile pin order is appearance metadata,
-- never canonical identity. The account-level list row exposes bounded
-- loadActiveStories progress without storing Telegram content.

ALTER TABLE story_appearances
    ADD COLUMN profile_pin_order INTEGER
        CHECK (profile_pin_order IS NULL OR profile_pin_order >= 0);

CREATE UNIQUE INDEX story_appearances_profile_pin_order
    ON story_appearances (
        account_id, namespace_version, poster_chat_id, profile_pin_order
    )
    WHERE location = 'month' AND removed_at_ms IS NULL
      AND profile_pin_order IS NOT NULL;

CREATE TABLE story_content_locators (
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    poster_chat_id      INTEGER NOT NULL,
    story_id            INTEGER NOT NULL,
    role                TEXT NOT NULL CHECK (role <> ''),
    file_type           TEXT NOT NULL CHECK (file_type IN (
        'fileTypePhotoStory', 'fileTypeVideoStory', 'fileTypeThumbnail'
    )),
    is_primary          INTEGER NOT NULL CHECK (is_primary IN (0, 1)),
    local_file_id       INTEGER,
    remote_file_id      TEXT CHECK (remote_file_id IS NULL OR remote_file_id <> ''),
    remote_unique_id    TEXT CHECK (remote_unique_id IS NULL OR remote_unique_id <> ''),
    size                INTEGER CHECK (size IS NULL OR size >= 0),
    expected_size       INTEGER CHECK (expected_size IS NULL OR expected_size >= 0),
    content_version     TEXT NOT NULL CHECK (content_version <> ''),
    PRIMARY KEY (
        account_id, namespace_version, poster_chat_id, story_id, role
    ),
    FOREIGN KEY (account_id, namespace_version, poster_chat_id, story_id)
        REFERENCES stories (
            account_id, namespace_version, poster_chat_id, story_id
        ) ON DELETE CASCADE,
    CHECK (local_file_id IS NOT NULL OR remote_file_id IS NOT NULL)
) STRICT, WITHOUT ROWID;

CREATE UNIQUE INDEX story_content_locators_one_primary
    ON story_content_locators (
        account_id, namespace_version, poster_chat_id, story_id
    )
    WHERE is_primary = 1;

CREATE TRIGGER story_content_locators_access_guard_insert
BEFORE INSERT ON story_content_locators
WHEN NOT EXISTS (
    SELECT 1 FROM stories
    WHERE account_id = NEW.account_id
      AND namespace_version = NEW.namespace_version
      AND poster_chat_id = NEW.poster_chat_id
      AND story_id = NEW.story_id
      AND content_state = 'available'
      AND can_be_forwarded = 1
      AND availability = 'fetchable'
)
BEGIN
    SELECT RAISE(ABORT, 'story locator requires available save-permitted content');
END;

CREATE TRIGGER story_content_locators_access_guard_update
BEFORE UPDATE ON story_content_locators
WHEN NOT EXISTS (
    SELECT 1 FROM stories
    WHERE account_id = NEW.account_id
      AND namespace_version = NEW.namespace_version
      AND poster_chat_id = NEW.poster_chat_id
      AND story_id = NEW.story_id
      AND content_state = 'available'
      AND can_be_forwarded = 1
      AND availability = 'fetchable'
)
BEGIN
    SELECT RAISE(ABORT, 'story locator requires available save-permitted content');
END;

CREATE TRIGGER stories_redact_locators_after_update
AFTER UPDATE OF content_state, can_be_forwarded, availability ON stories
WHEN NEW.content_state <> 'available'
  OR NEW.can_be_forwarded = 0
  OR NEW.availability <> 'fetchable'
BEGIN
    DELETE FROM story_content_locators
    WHERE account_id = NEW.account_id
      AND namespace_version = NEW.namespace_version
      AND poster_chat_id = NEW.poster_chat_id
      AND story_id = NEW.story_id;
END;

CREATE TABLE story_list_progress (
    account_id          INTEGER NOT NULL,
    namespace_version   INTEGER NOT NULL CHECK (namespace_version >= 0),
    generation          INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    pages_loaded        INTEGER NOT NULL DEFAULT 0 CHECK (pages_loaded >= 0),
    complete            INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1)),
    updated_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (account_id, namespace_version),
    FOREIGN KEY (account_id)
        REFERENCES accounts (account_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

INSERT INTO story_list_progress (
    account_id, namespace_version, generation, pages_loaded, complete, updated_at_ms
)
SELECT account_id, namespace_version, 0, 0, 0, updated_at_ms
FROM accounts;

CREATE TRIGGER accounts_seed_story_list_progress
AFTER INSERT ON accounts
BEGIN
    INSERT INTO story_list_progress (
        account_id, namespace_version, generation, pages_loaded, complete, updated_at_ms
    ) VALUES (
        NEW.account_id, NEW.namespace_version, 0, 0, 0, NEW.updated_at_ms
    ) ON CONFLICT (account_id, namespace_version) DO NOTHING;
END;
