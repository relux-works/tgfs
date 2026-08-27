-- GramDrive state schema, version 25 (BUG-260822-123vht).
--
-- Installed acceptance chooses a small, uncached attachment before one Finder
-- hydration. Keep that foreground catalog bounded to live, fetchable attachment
-- rows and in size order; sorting every item in a large profile can otherwise
-- starve the very provider request the gate is intended to measure.

CREATE INDEX items_live_fetchable_attachments_by_size
    ON items (logical_size, item_id)
    WHERE kind = 'attachment'
      AND availability = 'fetchable'
      AND deleted_at_ms IS NULL
      AND logical_size > 0;
