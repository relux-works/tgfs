//! The item tree a script projects, and how a change batch moves it.
//!
//! One `apply` implementation, used twice: [`ScriptBuilder::build`] runs
//! every batch through it to validate the script, and
//! [`FakeSource::advance`] runs the same batches through it at test time.
//! Two implementations would let a script validate and then behave
//! differently — the exact failure a fixture must not have.
//!
//! [`ScriptBuilder::build`]: crate::ScriptBuilder::build
//! [`FakeSource::advance`]: crate::FakeSource::advance

// See the note in `crate::script`: `ScriptError` is deliberately large, and
// every function here runs once per scripted item at fixture-build time.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use gramdrive_model::identity::ItemId;
use gramdrive_source::{ItemChange, ItemContent, SourceItem};

use crate::script::ScriptError;

/// The tree at one revision.
///
/// Children are a `Vec` per parent, not a set: enumeration order is part of
/// what the fake must reproduce, so it is stored explicitly rather than
/// left to a map's iteration order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tree {
    items: HashMap<ItemId, SourceItem>,
    children: HashMap<ItemId, Vec<ItemId>>,
}

impl Tree {
    /// The item, if it exists at this revision.
    pub(crate) fn get(&self, id: &ItemId) -> Option<&SourceItem> {
        self.items.get(id)
    }

    /// The parent's children in enumeration order, or an empty slice for a
    /// parent with none.
    pub(crate) fn children_of(&self, parent: &ItemId) -> &[ItemId] {
        self.children.get(parent).map_or(&[], Vec::as_slice)
    }

    /// Inserts one item, as a base-revision entry.
    ///
    /// Rejects a duplicate identity and an unknown or non-directory parent:
    /// a script whose base tree is not a tree cannot be scripted honestly.
    pub(crate) fn insert(&mut self, item: SourceItem) -> Result<(), ScriptError> {
        if self.items.contains_key(&item.id) {
            return Err(ScriptError::DuplicateItem {
                item: item.id.clone(),
            });
        }
        self.link(item)
    }

    /// Applies one change event, moving the tree forward one step.
    pub(crate) fn apply(&mut self, change: ItemChange) -> Result<(), ScriptError> {
        match change {
            ItemChange::Upserted(item) => self.upsert(item),
            ItemChange::Removed(id) => self.remove(&id),
        }
    }

    fn upsert(&mut self, item: SourceItem) -> Result<(), ScriptError> {
        let Some(existing) = self.items.get(&item.id) else {
            return self.link(item);
        };

        // Rootness is structural, so it cannot be edited: a child cannot
        // become parentless and the root cannot acquire a parent. Checked
        // here rather than in `link`, which sees only one item and cannot
        // know whether a parentless one is *the* root or a second one.
        if existing.parent.is_none() != item.parent.is_none() {
            return Err(ScriptError::RootReparented);
        }

        // A re-upsert with the same parent keeps its position: metadata
        // edits must not silently reshuffle a listing, or a test could not
        // tell an edit from a move.
        if existing.parent == item.parent {
            self.items.insert(item.id.clone(), item);
            return Ok(());
        }

        // Both parents are `Some` past the rootness check above: a moved
        // item leaves its old sibling list and joins the new one.
        if let Some(previous) = existing.parent.clone()
            && let Some(siblings) = self.children.get_mut(&previous)
        {
            siblings.retain(|sibling| sibling != &item.id);
        }
        self.items.remove(&item.id);
        self.link(item)
    }

    /// Inserts an item and attaches it to its parent's child list.
    fn link(&mut self, item: SourceItem) -> Result<(), ScriptError> {
        match &item.parent {
            None => {
                // Rootness is checked by the script validator, which knows
                // which id is the declared root; here it is enough that a
                // parentless item has no listing to join.
            }
            Some(parent) => {
                let Some(parent_item) = self.items.get(parent) else {
                    return Err(ScriptError::UnknownParent {
                        item: item.id.clone(),
                        parent: parent.clone(),
                    });
                };
                if !matches!(parent_item.content, ItemContent::Directory(_)) {
                    return Err(ScriptError::ParentNotDirectory {
                        item: item.id.clone(),
                        parent: parent.clone(),
                    });
                }
                self.children
                    .entry(parent.clone())
                    .or_default()
                    .push(item.id.clone());
            }
        }
        self.items.insert(item.id.clone(), item);
        Ok(())
    }

    /// Removes an item and everything beneath it.
    ///
    /// The subtree goes too. A source that removed a chat folder but left
    /// its files reachable would be a tree with a hole in it — `children`
    /// of the removed directory would fail with `NotFound` while a `fetch`
    /// of a file inside it still succeeded, which is not a state any real
    /// source presents and not one worth teaching a test to expect.
    fn remove(&mut self, id: &ItemId) -> Result<(), ScriptError> {
        let Some(item) = self.items.remove(id) else {
            return Err(ScriptError::RemovedUnknownItem { item: id.clone() });
        };
        if let Some(parent) = &item.parent
            && let Some(siblings) = self.children.get_mut(parent)
        {
            siblings.retain(|sibling| sibling != id);
        }
        let mut doomed = self.children.remove(id).unwrap_or_default();
        while let Some(descendant) = doomed.pop() {
            self.items.remove(&descendant);
            if let Some(grandchildren) = self.children.remove(&descendant) {
                doomed.extend(grandchildren);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use gramdrive_source::{DirectoryKind, FileKind};

    fn base() -> Tree {
        let scope = fixture::scope();
        let mut tree = Tree::default();
        tree.insert(
            fixture::directory(
                fixture::account_root_id(scope),
                None,
                "Account",
                "m1",
                DirectoryKind::Root,
            )
            .unwrap(),
        )
        .unwrap();
        tree.insert(
            fixture::directory(
                fixture::chat_id(scope, 100),
                Some(fixture::account_root_id(scope)),
                "Team",
                "m2",
                DirectoryKind::Chat,
            )
            .unwrap(),
        )
        .unwrap();
        tree
    }

    fn attachment(name: &str, version: &str) -> SourceItem {
        let scope = fixture::scope();
        fixture::file(
            fixture::attachment_id(scope, 100, 5, 0),
            fixture::chat_id(scope, 100),
            name,
            version,
            "c1",
            8,
            FileKind::Attachment,
        )
        .unwrap()
    }

    #[test]
    fn insert_links_children_in_order() {
        let scope = fixture::scope();
        let mut tree = base();
        for (index, chat) in [101, 102, 103].into_iter().enumerate() {
            tree.insert(
                fixture::directory(
                    fixture::chat_id(scope, chat),
                    Some(fixture::account_root_id(scope)),
                    &format!("Chat {index}"),
                    "m3",
                    DirectoryKind::Chat,
                )
                .unwrap(),
            )
            .unwrap();
        }
        let listing = tree.children_of(&fixture::account_root_id(scope));
        assert_eq!(
            listing,
            &[
                fixture::chat_id(scope, 100),
                fixture::chat_id(scope, 101),
                fixture::chat_id(scope, 102),
                fixture::chat_id(scope, 103),
            ],
            "insertion order is enumeration order"
        );
    }

    #[test]
    fn insert_rejects_a_duplicate_identity() {
        let scope = fixture::scope();
        let mut tree = base();
        let error = tree
            .insert(
                fixture::directory(
                    fixture::chat_id(scope, 100),
                    Some(fixture::account_root_id(scope)),
                    "Duplicate",
                    "m9",
                    DirectoryKind::Chat,
                )
                .unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, ScriptError::DuplicateItem { .. }));
    }

    #[test]
    fn insert_rejects_an_unknown_parent() {
        let scope = fixture::scope();
        let mut tree = base();
        let error = tree
            .insert(
                fixture::directory(
                    fixture::year_dir_id(scope, 999, 2026),
                    Some(fixture::chat_id(scope, 999)),
                    "Orphan",
                    "m9",
                    DirectoryKind::Year,
                )
                .unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, ScriptError::UnknownParent { .. }));
    }

    #[test]
    fn insert_rejects_a_file_as_parent() {
        let scope = fixture::scope();
        let mut tree = base();
        tree.insert(attachment("photo.jpg", "m3")).unwrap();
        let error = tree
            .insert(
                fixture::directory(
                    fixture::year_dir_id(scope, 100, 2026),
                    Some(fixture::attachment_id(scope, 100, 5, 0)),
                    "Inside a file",
                    "m9",
                    DirectoryKind::Year,
                )
                .unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, ScriptError::ParentNotDirectory { .. }));
    }

    #[test]
    fn upsert_in_place_keeps_listing_position() {
        let scope = fixture::scope();
        let mut tree = base();
        tree.insert(attachment("photo.jpg", "m3")).unwrap();
        tree.insert(
            fixture::file(
                fixture::attachment_id(scope, 100, 5, 1),
                fixture::chat_id(scope, 100),
                "second.jpg",
                "m4",
                "c1",
                8,
                FileKind::Attachment,
            )
            .unwrap(),
        )
        .unwrap();

        let before = tree.children_of(&fixture::chat_id(scope, 100)).to_vec();
        tree.apply(ItemChange::Upserted(attachment("renamed.jpg", "m5")))
            .unwrap();
        assert_eq!(
            tree.children_of(&fixture::chat_id(scope, 100)),
            before.as_slice(),
            "a metadata edit must not reshuffle siblings"
        );
        assert_eq!(
            tree.get(&fixture::attachment_id(scope, 100, 5, 0))
                .unwrap()
                .display_name,
            "renamed.jpg"
        );
    }

    #[test]
    fn upsert_with_a_new_parent_moves_the_item() {
        let scope = fixture::scope();
        let mut tree = base();
        tree.insert(
            fixture::directory(
                fixture::chat_id(scope, 101),
                Some(fixture::account_root_id(scope)),
                "Other",
                "m3",
                DirectoryKind::Chat,
            )
            .unwrap(),
        )
        .unwrap();
        tree.insert(attachment("photo.jpg", "m4")).unwrap();

        let mut moved = attachment("photo.jpg", "m5");
        moved.parent = Some(fixture::chat_id(scope, 101));
        tree.apply(ItemChange::Upserted(moved)).unwrap();

        assert!(
            tree.children_of(&fixture::chat_id(scope, 100)).is_empty(),
            "the old parent no longer lists it"
        );
        assert_eq!(
            tree.children_of(&fixture::chat_id(scope, 101)),
            &[fixture::attachment_id(scope, 100, 5, 0)],
            "the new parent does"
        );
    }

    #[test]
    fn remove_detaches_from_the_parent_listing() {
        let scope = fixture::scope();
        let mut tree = base();
        tree.insert(attachment("photo.jpg", "m3")).unwrap();
        tree.apply(ItemChange::Removed(fixture::attachment_id(
            scope, 100, 5, 0,
        )))
        .unwrap();
        assert!(
            tree.get(&fixture::attachment_id(scope, 100, 5, 0))
                .is_none()
        );
        assert!(tree.children_of(&fixture::chat_id(scope, 100)).is_empty());
    }

    #[test]
    fn remove_takes_the_whole_subtree() {
        let scope = fixture::scope();
        let mut tree = base();
        tree.insert(
            fixture::directory(
                fixture::year_dir_id(scope, 100, 2026),
                Some(fixture::chat_id(scope, 100)),
                "2026",
                "m3",
                DirectoryKind::Year,
            )
            .unwrap(),
        )
        .unwrap();
        tree.insert(
            fixture::file(
                fixture::attachment_id(scope, 100, 5, 0),
                fixture::year_dir_id(scope, 100, 2026),
                "photo.jpg",
                "m4",
                "c1",
                8,
                FileKind::Attachment,
            )
            .unwrap(),
        )
        .unwrap();

        tree.apply(ItemChange::Removed(fixture::chat_id(scope, 100)))
            .unwrap();

        assert!(tree.get(&fixture::chat_id(scope, 100)).is_none());
        assert!(
            tree.get(&fixture::year_dir_id(scope, 100, 2026)).is_none(),
            "descendants go with the removed directory"
        );
        assert!(
            tree.get(&fixture::attachment_id(scope, 100, 5, 0))
                .is_none(),
            "no file outlives its removed ancestor"
        );
    }

    #[test]
    fn remove_rejects_an_unknown_item() {
        let scope = fixture::scope();
        let mut tree = base();
        let error = tree
            .apply(ItemChange::Removed(fixture::chat_id(scope, 999)))
            .unwrap_err();
        assert!(matches!(error, ScriptError::RemovedUnknownItem { .. }));
    }

    #[test]
    fn upserting_an_item_into_rootlessness_is_rejected() {
        let scope = fixture::scope();
        let mut tree = base();
        let mut orphaned = tree.get(&fixture::chat_id(scope, 100)).unwrap().clone();
        orphaned.parent = None;
        let error = tree.apply(ItemChange::Upserted(orphaned)).unwrap_err();
        assert!(matches!(error, ScriptError::RootReparented));
    }
}
