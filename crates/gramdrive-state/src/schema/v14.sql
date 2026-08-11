-- GramDrive state schema, version 14 (BUG-260727-2ct7o0).
--
-- The generated-document worklist used to sort only by opaque item identity.
-- A low-sorting chat that was dirtied by every incoming history page could
-- therefore be rendered repeatedly while never-published documents from
-- ordinary chats waited behind it. Order first by the durable publication
-- watermark and publication time: never-published and least-advanced
-- documents lead, while a document just refreshed rotates behind older work.

DROP INDEX render_state_dirty;

CREATE INDEX render_state_dirty_fair
    ON render_state (input_watermark_seq, rendered_at_ms, item_id)
    WHERE dirty = 1;
