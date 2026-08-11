-- A chat-level render catalog consists only of direct .chat.json children.
-- Keep unrelated generated documents out of the lookup on large accounts.
CREATE INDEX items_live_generated_docs_by_parent
    ON items (parent_item_id, item_id)
    WHERE kind = 'generated_doc' AND deleted_at_ms IS NULL;
