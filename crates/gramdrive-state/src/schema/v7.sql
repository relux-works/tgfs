-- GramDrive state schema, version 7 (TASK-260721-ddqgxa).
--
-- TDLib's process-local numeric file id is the locator accepted by
-- downloadFile. It is distinct from remote.id and remote.unique_id, so it is
-- persisted separately instead of overloading either existing text field.

ALTER TABLE attachments
    ADD COLUMN telegram_local_file_id INTEGER
        CHECK (telegram_local_file_id IS NULL OR telegram_local_file_id > 0);
