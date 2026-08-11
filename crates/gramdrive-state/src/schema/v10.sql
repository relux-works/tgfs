-- Story appearance identity includes Active/month location. A transition
-- retains the old provider tombstone while publishing the new monthly item,
-- so the generic one-appearance-per-canonical/view index must exclude this
-- one location-scoped item kind. ItemId remains unique and encodes location.
DROP INDEX items_appearance;
CREATE UNIQUE INDEX items_appearance
    ON items (canonical_item_id, view_kind, COALESCE(view_folder_id, 0))
    WHERE canonical_item_id IS NOT NULL AND kind <> 'story_appearance';
CREATE INDEX items_by_canonical_item
    ON items (canonical_item_id, item_id)
    WHERE canonical_item_id IS NOT NULL;
