-- GramDrive state schema, version 23 (BUG-260822-123vht).
--
-- Generated render publication reclaims immutable generations only after no
-- current cache row owns their path. That lookup runs inside the narrow
-- generated-file lease critical section, so it must scale with one path rather
-- than with every cached item in a large profile. NULL rows cannot claim a
-- materialization and are excluded from the index.

CREATE INDEX cache_entries_by_materialization_ref
    ON cache_entries (materialization_ref)
    WHERE materialization_ref IS NOT NULL;
