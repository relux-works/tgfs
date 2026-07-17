-- GramDrive migration journal (TASK-260715-18l9xz).
--
-- The runner's own bookkeeping, deliberately *outside* the numbered schema
-- sequence. Two facts force that:
--
-- * A database written by a build older than the runner has no journal and
--   must still be migratable. The journal cannot be introduced by a
--   migration, because the runner needs it to run one.
-- * Nothing here is user data. A migration migrates the schema in
--   `v1.sql` and everything after it; it never migrates these tables.
--
-- So this script is applied with IF NOT EXISTS to every database this build
-- opens, at whatever version it is on, once the version is known to be one
-- this build may write to. It is not versioned: any change here has to stay
-- compatible with every journal these statements ever created.
--
-- `schema_history` is the other half of the story and lives in v1.sql,
-- because it *is* part of the v1 schema: user_version says what is current,
-- schema_history says how the file got there (SYNC-072, NFR-041).

-- ---------------------------------------------------------------------------
-- migration_progress — the durable checkpoint of a resumable migration, and
-- the reason SYNC-072 holds.
--
-- One row exists exactly while a resumable migration to `version` has
-- committed chunks but has not finished: the runner writes the row in the
-- same transaction as the chunk's data changes, and deletes it in the same
-- transaction that stamps user_version. So the row and the version can never
-- disagree — a crash leaves user_version at the old version and this row
-- pointing at the last chunk that actually committed, which is exactly where
-- the next run resumes.
--
-- checkpoint is opaque to the runner: the migration that wrote it is the only
-- thing that interprets it. chunks_done is diagnostics — how far a long
-- migration got before it was interrupted, and whether a resume is making
-- progress across restarts.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS migration_progress (
    version       INTEGER NOT NULL PRIMARY KEY CHECK (version > 0),
    name          TEXT    NOT NULL CHECK (name <> ''),
    checkpoint    TEXT    NOT NULL,
    chunks_done   INTEGER NOT NULL CHECK (chunks_done > 0),
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

-- ---------------------------------------------------------------------------
-- repair_markers — durable "this file needs a repair pass" notes (SYNC-071,
-- NFR-034).
--
-- A marker is a handoff, not an error: the thing that raises it cannot or
-- should not do the work inline, and the thing that clears it is
-- reconciliation. A migration that changes the shape of a rebuildable
-- projection raises one rather than rebuilding 100k rows inside a schema
-- upgrade; the runner raises one while a resumable migration has an
-- uncommitted tail.
--
-- (kind, detail) is the marker's identity, so raising the same marker twice
-- leaves one row with the *first* raised_at_ms — on a resume that is the
-- interruption's timestamp, which is the one worth keeping. Nothing here
-- cascades: a marker outlives the rows that caused it, which is the point.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS repair_markers (
    marker_id    INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    kind         TEXT    NOT NULL CHECK (kind <> ''),
    detail       TEXT    NOT NULL,
    raised_at_ms INTEGER NOT NULL CHECK (raised_at_ms >= 0),
    UNIQUE (kind, detail)
) STRICT;
