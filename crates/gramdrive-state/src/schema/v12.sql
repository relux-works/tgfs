-- GramDrive state schema, version 12 (TASK-260721-2tamdj).
--
-- Audit retention needs a version-addressable owner after the live
-- attachment row advances to a newer Telegram content version. This table
-- deliberately omits every downloadable locator: it can retain observed
-- metadata and already materialized verified bytes, but can never create
-- network demand for a historical version.

CREATE TABLE retained_attachment_versions (
    account_id             INTEGER NOT NULL
        REFERENCES accounts (account_id) ON DELETE CASCADE,
    item_id                BLOB    NOT NULL CHECK (length(item_id) > 0),
    content_version        TEXT    NOT NULL CHECK (content_version <> ''),
    logical_kind           TEXT    NOT NULL CHECK (logical_kind <> ''),
    telegram_representation TEXT  NOT NULL CHECK (telegram_representation <> ''),
    fidelity               TEXT    NOT NULL CHECK (fidelity <> ''),
    source_name            TEXT CHECK (source_name IS NULL OR source_name <> ''),
    mime_type              TEXT CHECK (mime_type IS NULL OR mime_type <> ''),
    exact_size             INTEGER CHECK (exact_size IS NULL OR exact_size >= 0),
    telegram_unique_id     TEXT CHECK (
        telegram_unique_id IS NULL OR telegram_unique_id <> ''
    ),
    blob_hash_algo         TEXT CHECK (blob_hash_algo IN ('sha256')),
    blob_hash              BLOB,
    last_verified_at_ms    INTEGER,
    materialized_size      INTEGER CHECK (
        materialized_size IS NULL OR materialized_size >= 0
    ),
    materialization_ref    TEXT CHECK (
        materialization_ref IS NULL OR materialization_ref <> ''
    ),
    retained_at_ms         INTEGER NOT NULL,
    PRIMARY KEY (account_id, item_id, content_version),
    FOREIGN KEY (account_id, blob_hash_algo, blob_hash)
        REFERENCES blobs (account_id, hash_algo, hash),
    CHECK ((blob_hash IS NULL) = (blob_hash_algo IS NULL)),
    CHECK (
        (materialization_ref IS NULL)
        = (materialized_size IS NULL)
    ),
    CHECK (
        materialization_ref IS NULL
        OR blob_hash IS NOT NULL
    )
) STRICT, WITHOUT ROWID;

CREATE INDEX retained_attachment_versions_by_blob
    ON retained_attachment_versions (account_id, blob_hash_algo, blob_hash)
    WHERE blob_hash IS NOT NULL;

CREATE INDEX retained_attachment_versions_by_materialization
    ON retained_attachment_versions (materialization_ref)
    WHERE materialization_ref IS NOT NULL;
