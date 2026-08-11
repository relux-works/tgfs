-- A policy-refused generated document must leave a durable, countable record
-- when it is removed from the bounded dirty worklist.  Requeueing and normal
-- publication clear both fields in the repository layer.
ALTER TABLE render_state ADD COLUMN skip_reason TEXT
    CHECK (skip_reason IS NULL OR skip_reason = 'policy-excluded');
ALTER TABLE render_state ADD COLUMN skipped_at_ms INTEGER;

CREATE INDEX render_state_policy_excluded
    ON render_state (skipped_at_ms, item_id)
    WHERE skip_reason = 'policy-excluded';
