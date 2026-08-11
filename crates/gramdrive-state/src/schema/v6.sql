-- GramDrive state schema, version 6 (TASK-260721-1dzolg).
--
-- Message event watermarks advance only when normalized message state changes.
-- Rendering policy can also change bytes without appending an event, so each
-- account carries an independent monotonic generation. Retention/timezone
-- transitions increment it in the same transaction that dirties documents.

ALTER TABLE accounts
    ADD COLUMN render_generation INTEGER NOT NULL DEFAULT 0
        CHECK (render_generation >= 0);
