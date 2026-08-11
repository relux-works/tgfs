-- GramDrive state schema, version 17 (BUG-260728-2qfzbd).
--
-- Backward history crawling is scheduled least-recently-first, and until now
-- the key it sorted on was `chat_sync_state.last_sync_at_ms` — a column both
-- the history crawl *and* the live-update path stamp. A chat that keeps
-- receiving messages therefore had its place in the queue reset by traffic it
-- never crawled, and sank back to the end of the rotation every time. The
-- busier a correspondence is, the less history it gets: exactly inverted.
--
-- On a preserved profile this was not a slow rotation, it was a stall. The
-- reported chat sat at an unmoved backward frontier for over an hour while the
-- account indexed more than a hundred thousand messages from quieter chats,
-- and the same held for the other two sampled active chats. That is the
-- "without starvation" clause of this bug, and it is a second, independent
-- cause from the self-fenced-chat exclusion fixed alongside it.
--
-- So the scheduling key becomes its own fact: `last_backfill_at_ms`, written
-- only when a chat is actually handed a history turn. `last_sync_at_ms` keeps
-- its own meaning — when anything was last observed for this chat — and keeps
-- its live-path writer; it simply stops deciding who crawls next.
--
-- Existing rows start NULL, which sorts first: every incomplete chat is
-- guaranteed one turn before any chat gets a second. That is the bounded,
-- self-correcting repair for profiles that starved, and it costs one rotation.
--
-- Fairness precedent in this schema: v14 did the same for the generated-render
-- worklist, where recently refreshed chats monopolized publication.

ALTER TABLE chat_sync_state ADD COLUMN last_backfill_at_ms INTEGER;

-- The backlog now orders by the new column, so it needs the index the old
-- one had. Same shape as `chat_sync_state_backlog` (v5): scope, key, then
-- chat_id for the deterministic tie-break, partial on the incomplete rows the
-- scheduler actually reads.
CREATE INDEX chat_sync_state_backfill_turns
    ON chat_sync_state
       (account_id, namespace_version, last_backfill_at_ms, chat_id)
    WHERE history_complete = 0;

-- The old index has no remaining reader, and it was maintained on every live
-- message: `last_sync_at_ms` moves whenever a chat receives anything. Dropping
-- it removes that write amplification from the hot live path rather than
-- leaving a second index nothing queries.
DROP INDEX chat_sync_state_backlog;
