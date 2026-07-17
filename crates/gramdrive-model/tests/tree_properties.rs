//! Property suite for the virtual tree builder (TASK-260715-3tjduq).
//!
//! Proves the order-independence acceptance criterion over sampled record
//! space: shuffling every input collection yields a byte-identical tree,
//! chunked enumeration equals full enumeration for any page size
//! (SYNC-003), node identities are globally unique, and every enumerated
//! node resolves back to itself through its `ItemId`.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use gramdrive_model::identity::{
    AccountId, AttachmentIndex, ChatId, ChatListKind, ContentHash, FolderId, ItemId, MessageId,
    NamespaceVersion, SchemaFamily,
};
use gramdrive_model::tree::{
    AccountRecord, AttachmentRecord, ChatRecord, ChildrenError, DocSchemas, FolderRecord,
    MonthStamp, TreeProjection,
};
use proptest::collection::{btree_map, btree_set};
use proptest::option;
use proptest::prelude::*;

fn schemas() -> DocSchemas {
    DocSchemas {
        chat_json: SchemaFamily(1),
        messages_ndjson: SchemaFamily(2),
        month_markdown: SchemaFamily(3),
        order_json: SchemaFamily(4),
    }
}

fn arb_month() -> impl Strategy<Value = MonthStamp> {
    (2020u16..2030, 1u8..=12).prop_map(|(year, month)| MonthStamp { year, month })
}

fn arb_attachments() -> impl Strategy<Value = Vec<AttachmentRecord>> {
    btree_map(
        (-50i64..50, 0u32..3),
        (arb_month(), option::of(any::<u8>())),
        0..4,
    )
    .prop_map(|map| {
        map.into_iter()
            .map(|((message_id, index), (month, content))| AttachmentRecord {
                message_id: MessageId(message_id),
                index: AttachmentIndex(index),
                month,
                display_name: format!("file-{message_id}-{index}.bin"),
                size: Some(u64::from(index) + 1),
                content: content.map(|byte| ContentHash::Sha256([byte; 32])),
            })
            .collect()
    })
}

/// Membership flags: Main, Archive, and a subset of the folder IDs the
/// account actually has — dangling folder memberships are an input error,
/// not a sampling target.
fn arb_memberships(folder_ids: Vec<i32>) -> impl Strategy<Value = Vec<ChatListKind>> {
    let folder_count = folder_ids.len();
    (
        any::<bool>(),
        any::<bool>(),
        proptest::collection::vec(any::<bool>(), folder_count),
    )
        .prop_map(move |(main, archive, folder_flags)| {
            let mut memberships = Vec::new();
            if main {
                memberships.push(ChatListKind::Main);
            }
            if archive {
                memberships.push(ChatListKind::Archive);
            }
            for (folder_id, keep) in folder_ids.iter().zip(folder_flags) {
                if keep {
                    memberships.push(ChatListKind::Folder(FolderId(*folder_id)));
                }
            }
            memberships
        })
}

fn arb_chat(chat_id: i64, folder_ids: Vec<i32>) -> impl Strategy<Value = ChatRecord> {
    (
        arb_memberships(folder_ids),
        btree_set(arb_month(), 0..4),
        arb_attachments(),
        option::of("[a-z]{1,8}"),
    )
        .prop_map(
            move |(memberships, months, attachments, username)| ChatRecord {
                chat_id: ChatId(chat_id),
                title: format!("Chat {chat_id}"),
                username,
                memberships,
                message_months: months.into_iter().collect(),
                attachments,
            },
        )
}

fn arb_input() -> impl Strategy<Value = (Vec<FolderRecord>, Vec<ChatRecord>)> {
    btree_set(-20i32..20, 0..3).prop_flat_map(|folder_ids| {
        let folder_ids: Vec<i32> = folder_ids.into_iter().collect();
        let folders: Vec<FolderRecord> = folder_ids
            .iter()
            .map(|id| FolderRecord {
                folder_id: FolderId(*id),
                title: format!("Folder {id}"),
            })
            .collect();
        let chats = btree_set(-100i64..100, 0..4).prop_flat_map(move |chat_ids| {
            let strategies: Vec<_> = chat_ids
                .into_iter()
                .map(|chat_id| arb_chat(chat_id, folder_ids.clone()))
                .collect();
            strategies
        });
        (Just(folders), chats)
    })
}

/// Deterministic Fisher–Yates driven by a caller-provided seed — proptest
/// supplies the entropy, so runs stay reproducible.
fn shuffle<T>(items: &mut [T], seed: &mut u64) {
    for i in (1..items.len()).rev() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let j = (*seed % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

fn shuffled_input(
    folders: &[FolderRecord],
    chats: &[ChatRecord],
    mut seed: u64,
) -> (Vec<FolderRecord>, Vec<ChatRecord>) {
    let mut folders = folders.to_vec();
    shuffle(&mut folders, &mut seed);
    let mut chats = chats.to_vec();
    shuffle(&mut chats, &mut seed);
    for chat in &mut chats {
        shuffle(&mut chat.memberships, &mut seed);
        shuffle(&mut chat.message_months, &mut seed);
        shuffle(&mut chat.attachments, &mut seed);
    }
    (folders, chats)
}

fn build(
    folders: Vec<FolderRecord>,
    chats: Vec<ChatRecord>,
) -> Result<TreeProjection, TestCaseError> {
    TreeProjection::new(
        AccountRecord {
            account_id: AccountId(7),
            namespace_version: NamespaceVersion(3),
            display_name: "Account".to_string(),
        },
        folders,
        chats,
        schemas(),
    )
    .map_err(|error| TestCaseError::fail(error.to_string()))
}

/// One enumerated node: id text, parent id text, display name, is-directory.
type NodeRow = (String, Option<String>, String, bool);

/// Full depth-first enumeration with the given page size, in tree order.
fn walk(tree: &TreeProjection, page_size: NonZeroUsize) -> Result<Vec<NodeRow>, ChildrenError> {
    let root = tree.root();
    let mut rows = vec![(
        root.id.text(),
        None,
        root.display_name.clone(),
        root.kind.is_directory(),
    )];
    walk_into(tree, &root.id, page_size, &mut rows)?;
    Ok(rows)
}

fn walk_into(
    tree: &TreeProjection,
    parent: &ItemId,
    page_size: NonZeroUsize,
    rows: &mut Vec<NodeRow>,
) -> Result<(), ChildrenError> {
    let mut after: Option<ItemId> = None;
    loop {
        let chunk = tree.children(parent, after.as_ref(), page_size)?;
        for node in &chunk.nodes {
            rows.push((
                node.id.text(),
                node.parent.as_ref().map(ItemId::text),
                node.display_name.clone(),
                node.kind.is_directory(),
            ));
            if node.kind.is_directory() {
                walk_into(tree, &node.id, page_size, rows)?;
            }
        }
        match chunk.next {
            Some(boundary) => after = Some(boundary),
            None => return Ok(()),
        }
    }
}

fn big_page() -> NonZeroUsize {
    NonZeroUsize::new(4096).unwrap_or(NonZeroUsize::MIN)
}

proptest! {
    /// The acceptance criterion itself: shuffling every input collection —
    /// folders, chats, memberships, months, attachments — produces an
    /// identical tree, node for node, id for id, name for name.
    #[test]
    fn output_is_deterministic_under_shuffled_input(
        (folders, chats) in arb_input(),
        seed_a in any::<u64>(),
        seed_b in any::<u64>(),
    ) {
        let (folders_a, chats_a) = shuffled_input(&folders, &chats, seed_a);
        let (folders_b, chats_b) = shuffled_input(&folders, &chats, seed_b);
        let tree_a = build(folders_a, chats_a)?;
        let tree_b = build(folders_b, chats_b)?;
        prop_assert_eq!(walk(&tree_a, big_page()), walk(&tree_b, big_page()));
    }

    /// SYNC-003: for any page size, chunked enumeration visits exactly the
    /// children full enumeration visits — no duplicates, no gaps.
    #[test]
    fn any_page_size_enumerates_exactly_once(
        (folders, chats) in arb_input(),
        page_size in 1usize..5,
    ) {
        let tree = build(folders, chats)?;
        let full = walk(&tree, big_page()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let paged = walk(
            &tree,
            NonZeroUsize::new(page_size).unwrap_or(NonZeroUsize::MIN),
        )
        .map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(full, paged);
    }

    /// Every node has a globally unique identity, and resolving that
    /// identity returns the same node the enumerator produced.
    #[test]
    fn ids_are_unique_and_resolve_back((folders, chats) in arb_input()) {
        let tree = build(folders, chats)?;
        let rows = walk(&tree, big_page()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let unique: BTreeSet<&String> = rows.iter().map(|(id, ..)| id).collect();
        prop_assert_eq!(unique.len(), rows.len(), "duplicate node identity");

        for (id_text, parent, name, is_dir) in &rows {
            let id = ItemId::parse_text(id_text)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let node = tree.node(&id);
            prop_assert!(node.is_some(), "enumerated id must resolve: {}", name);
            if let Some(node) = node {
                prop_assert_eq!(&node.id.text(), id_text);
                prop_assert_eq!(&node.parent.as_ref().map(ItemId::text), parent);
                prop_assert_eq!(&node.display_name, name);
                prop_assert_eq!(node.kind.is_directory(), *is_dir);
            }
        }
    }
}
