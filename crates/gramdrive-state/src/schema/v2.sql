-- GramDrive state schema, version 2 (TASK-260715-rhcnhc).
--
-- The provider-visible item change journal: the durable "what changed since
-- anchor N" read native file-system providers page their change enumeration
-- from (PLAT-MAC-004 change signaling; DOM-004's durable-cursor discipline
-- applied to the provider boundary). `data_version` answers only "did
-- anything change?" and is connection-relative by contract — it can never be
-- persisted as a sync anchor. This journal is what can.
--
-- Applied as one atomic migration by the runner in gramdrive-state
-- (src/migrate.rs); a database that was ever at v2 carries exactly what this
-- script says. Frozen once shipped, like schema/v1.sql (NFR-041).

-- ---------------------------------------------------------------------------
-- item_change_journal — one row: the journal's identity.
--
-- instance_id names this journal's sequence space. Sequence numbers are
-- meaningful only within one database life: recovery quarantines a corrupt
-- file and a fresh one starts its sequences over, so an anchor from the old
-- file must be recognizably foreign rather than silently pointing at
-- unrelated changes. SQLite's own randomblob seeds the value at migration
-- time; it is never rewritten.
-- ---------------------------------------------------------------------------
CREATE TABLE item_change_journal (
    id          INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    instance_id TEXT    NOT NULL CHECK (instance_id <> '')
) STRICT;

INSERT INTO item_change_journal (id, instance_id)
VALUES (1, lower(hex(randomblob(16))));

-- ---------------------------------------------------------------------------
-- item_changes — the coalesced journal: one row per item, carrying the
-- sequence of that item's *latest* provider-visible change.
--
-- Coalescing is what keeps the table bounded by item count instead of
-- change count, and it loses nothing a provider needs: change enumeration
-- replays current item state, not history, so an anchor taken before any of
-- an item's changes still meets the item once, at its newest sequence.
-- Writers refresh a row by DELETE + INSERT; AUTOINCREMENT guarantees the
-- new sequence is greater than every sequence ever issued (sqlite_sequence
-- never rewinds), which is exactly the property "changed after anchor N"
-- pages depend on.
--
-- account_id is denormalized from items so per-account paging can filter
-- and LIMIT in one indexed scan; the cascade keeps journal rows exactly as
-- long as their item rows (an account or epoch sweep takes both).
-- ---------------------------------------------------------------------------
CREATE TABLE item_changes (
    change_seq INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    item_id    BLOB    NOT NULL UNIQUE REFERENCES items (item_id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL
) STRICT;

-- Per-account change pages: WHERE account_id = ? AND change_seq > ?
-- ORDER BY change_seq LIMIT ?.
CREATE INDEX item_changes_by_account
    ON item_changes (account_id, change_seq);
