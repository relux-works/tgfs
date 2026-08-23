-- GramDrive state schema, version 24 (BUG-260729-28hnfq).
--
-- A successful namespace publication remains usable across agent restart
-- while source recovery and bounded projection convergence continue. This is
-- coordination metadata keyed by already-owned numeric account/chat scope:
-- no title, filename, message, content value, or secret is stored here.

CREATE TABLE namespace_readiness (
    account_id               INTEGER NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    namespace_version        INTEGER NOT NULL CHECK (namespace_version >= 0),
    generation               INTEGER NOT NULL CHECK (generation > 0),
    published_at_ms          INTEGER NOT NULL CHECK (published_at_ms >= 0),
    projection_after_chat_id INTEGER,
    convergence_complete     INTEGER NOT NULL CHECK (convergence_complete IN (0, 1)),
    updated_at_ms            INTEGER NOT NULL CHECK (updated_at_ms >= published_at_ms),
    PRIMARY KEY (account_id, namespace_version)
) STRICT;
