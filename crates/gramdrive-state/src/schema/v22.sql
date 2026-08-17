-- GramDrive state schema, version 22 (BUG-260729-1ib88y).
--
-- Secret bytes never enter this table. The keychain keeps the staged and
-- rollback keys under reserved account aliases; this row records only which
-- side of the single SQLite commit point recovery must preserve.

CREATE TABLE auth_finalization_journal (
    account_id       INTEGER NOT NULL PRIMARY KEY CHECK (account_id > 0),
    phase            TEXT    NOT NULL CHECK (phase IN ('prepared', 'committed')),
    had_account_row  INTEGER NOT NULL CHECK (had_account_row IN (0, 1)),
    had_database_key INTEGER NOT NULL CHECK (had_database_key IN (0, 1)),
    had_tdlib_state  INTEGER NOT NULL CHECK (had_tdlib_state IN (0, 1))
) STRICT;
