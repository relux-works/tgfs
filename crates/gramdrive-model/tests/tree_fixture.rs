//! Fixture suite for the virtual tree builder (TASK-260715-3tjduq).
//!
//! Pins the default layout of `.spec/sync-and-filesystem-semantics.md`
//! against literal expected listings, and exercises the acceptance
//! criteria that have single-case shapes: POL-1 stable names, multiple
//! appearances over one canonical record (PRD-013, SYNC-010), read-only
//! capabilities (DEC-007, SYNC-060), rename stability (SYNC-026), page
//! boundaries (SYNC-003), and input-contract violations.

use std::num::NonZeroUsize;

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, CanonicalKey, ChatId,
    ChatKey, ChatListKind, ContentHash, FolderId, ItemId, ItemKey, MessageId, NamespaceVersion,
    SchemaFamily,
};
use gramdrive_model::tree::{
    AccountRecord, AttachmentRecord, ChatRecord, ChildrenError, DocSchemas, FolderRecord,
    MonthStamp, NodeKind, TreeInputError, TreeProjection,
};

const JULY: MonthStamp = MonthStamp {
    year: 2026,
    month: 7,
};

fn schemas() -> DocSchemas {
    DocSchemas {
        chat_json: SchemaFamily(1),
        messages_ndjson: SchemaFamily(1),
        month_markdown: SchemaFamily(1),
        order_json: SchemaFamily(1),
    }
}

fn account() -> AccountRecord {
    AccountRecord {
        account_id: AccountId(42),
        namespace_version: NamespaceVersion(1),
        display_name: "Account".to_string(),
    }
}

/// The scope every fixture key lives in — mirrors [`account`].
fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(42),
        },
        namespace_version: NamespaceVersion(1),
    }
}

fn photo(message_id: i64) -> AttachmentRecord {
    AttachmentRecord {
        message_id: MessageId(message_id),
        index: AttachmentIndex(0),
        month: JULY,
        display_name: "photo.jpg".to_string(),
        size: Some(1234),
        content: Some(ContentHash::Sha256([0xaa; 32])),
    }
}

fn spec_chat() -> ChatRecord {
    ChatRecord {
        chat_id: ChatId(100),
        title: "Chat".to_string(),
        username: None,
        memberships: vec![ChatListKind::Main],
        message_months: vec![JULY],
        attachments: vec![photo(500)],
    }
}

fn build(
    folders: Vec<FolderRecord>,
    chats: Vec<ChatRecord>,
) -> Result<TreeProjection, TreeInputError> {
    TreeProjection::new(account(), folders, chats, schemas())
}

fn page(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN)
}

/// Renders the whole tree as an indented listing, enumerating every
/// directory page by page with the given page size.
fn walk_lines(
    tree: &TreeProjection,
    page_size: NonZeroUsize,
) -> Result<Vec<String>, ChildrenError> {
    let root = tree.root();
    let mut lines = vec![format!("{}/", root.display_name)];
    walk_into(tree, &root.id, 1, page_size, &mut lines)?;
    Ok(lines)
}

fn walk_into(
    tree: &TreeProjection,
    parent: &ItemId,
    depth: usize,
    page_size: NonZeroUsize,
    lines: &mut Vec<String>,
) -> Result<(), ChildrenError> {
    let mut after: Option<ItemId> = None;
    loop {
        let page = tree.children(parent, after.as_ref(), page_size)?;
        for node in &page.nodes {
            let indent = "  ".repeat(depth);
            if node.kind.is_directory() {
                lines.push(format!("{indent}{}/", node.display_name));
                walk_into(tree, &node.id, depth + 1, page_size, lines)?;
            } else {
                lines.push(format!("{indent}{}", node.display_name));
            }
        }
        match page.next {
            Some(boundary) => after = Some(boundary),
            None => return Ok(()),
        }
    }
}

/// The spec's tree-layout example, rendered exactly (SYNC-010 layout).
#[test]
fn fixture_tree_matches_spec_layout() {
    let tree = build(Vec::new(), vec![spec_chat()]).unwrap();
    let expected = [
        "Account/",
        "  Main/",
        "    order.json",
        "    Chat/",
        "      chat.json",
        "      messages.ndjson",
        "      2026/",
        "        07.md",
        "        media/",
        "          photo.jpg",
        "  Archive/",
        "    order.json",
        "  Telegram Folders/",
    ];
    assert_eq!(walk_lines(&tree, page(64)).unwrap(), expected);
}

/// Page size must not change what is enumerated, only how it is chunked.
#[test]
fn page_size_one_yields_the_same_tree() {
    let tree = build(Vec::new(), vec![spec_chat()]).unwrap();
    assert_eq!(
        walk_lines(&tree, page(1)).unwrap(),
        walk_lines(&tree, page(64)).unwrap()
    );
}

/// An account with no chats still exposes the three fixed roots.
#[test]
fn empty_account_has_fixed_roots() {
    let tree = build(Vec::new(), Vec::new()).unwrap();
    let expected = [
        "Account/",
        "  Main/",
        "    order.json",
        "  Archive/",
        "    order.json",
        "  Telegram Folders/",
    ];
    assert_eq!(walk_lines(&tree, page(64)).unwrap(), expected);
}

/// POL-1 stable chat folder name: `<Display Name> — @<username>`.
#[test]
fn chat_with_username_uses_pol1_name() {
    let mut chat = spec_chat();
    chat.username = Some("chatuser".to_string());
    chat.memberships = vec![ChatListKind::Folder(FolderId(7))];
    let tree = build(
        vec![FolderRecord {
            folder_id: FolderId(7),
            title: "Work".to_string(),
        }],
        vec![chat],
    )
    .unwrap();
    let lines = walk_lines(&tree, page(64)).unwrap();
    assert!(lines.contains(&"    Work/".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"      Chat — @chatuser/".to_string()),
        "{lines:?}"
    );
}

/// A year that only has media (no message months) still gets its directory,
/// and a month without attachments gets no media directory.
#[test]
fn media_and_month_partitions_are_independent() {
    let mut chat = spec_chat();
    chat.message_months = vec![MonthStamp {
        year: 2025,
        month: 3,
    }];
    let tree = build(Vec::new(), vec![chat]).unwrap();
    let expected = [
        "Account/",
        "  Main/",
        "    order.json",
        "    Chat/",
        "      chat.json",
        "      messages.ndjson",
        "      2025/",
        "        03.md",
        "      2026/",
        "        media/",
        "          photo.jpg",
        "  Archive/",
        "    order.json",
        "  Telegram Folders/",
    ];
    assert_eq!(walk_lines(&tree, page(64)).unwrap(), expected);
}

/// PRD-013/SYNC-010: one canonical chat in two views is two appearance
/// identities over one unchanged canonical record — including the shared
/// blob identity of its attachments.
#[test]
fn multiple_appearances_share_canonical_records_and_blobs() {
    let mut chat = spec_chat();
    chat.memberships = vec![ChatListKind::Main, ChatListKind::Folder(FolderId(7))];
    let tree = build(
        vec![FolderRecord {
            folder_id: FolderId(7),
            title: "Work".to_string(),
        }],
        vec![chat],
    )
    .unwrap();

    let chat_key = ChatKey {
        scope: scope(),
        chat_id: ChatId(100),
    };
    let in_main = tree
        .node(
            &ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Main,
                item: CanonicalKey::Chat(chat_key),
            })
            .id(),
        )
        .unwrap();
    let in_folder = tree
        .node(
            &ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Folder(FolderId(7)),
                item: CanonicalKey::Chat(chat_key),
            })
            .id(),
        )
        .unwrap();

    assert_ne!(in_main.id, in_folder.id, "appearances are distinct items");
    assert_eq!(
        in_main.canonical, in_folder.canonical,
        "one canonical record backs both"
    );
    assert_eq!(in_main.display_name, in_folder.display_name);

    // The subtrees mirror each other over identical canonical keys — and the
    // attachment's materialized bytes resolve to one shared blob identity.
    let canon_main = subtree_canonicals(&tree, &in_main.id);
    let canon_folder = subtree_canonicals(&tree, &in_folder.id);
    assert_eq!(canon_main, canon_folder);

    let blob_main = subtree_blobs(&tree, &in_main.id);
    let blob_folder = subtree_blobs(&tree, &in_folder.id);
    assert_eq!(blob_main.len(), 1);
    assert_eq!(blob_main, blob_folder);
}

fn subtree_canonicals(tree: &TreeProjection, parent: &ItemId) -> Vec<CanonicalKey> {
    let mut keys = Vec::new();
    let mut queue = vec![parent.clone()];
    while let Some(id) = queue.pop() {
        if let Ok(page) = tree.children(&id, None, page(64)) {
            for node in page.nodes {
                keys.push(node.canonical);
                if node.kind.is_directory() {
                    queue.push(node.id);
                }
            }
        }
    }
    keys.sort_by_key(|key| ItemKey::Canonical(*key).id().as_bytes().to_vec());
    keys
}

fn subtree_blobs(tree: &TreeProjection, parent: &ItemId) -> Vec<String> {
    let mut blobs = Vec::new();
    let mut queue = vec![parent.clone()];
    while let Some(id) = queue.pop() {
        if let Ok(page) = tree.children(&id, None, page(64)) {
            for node in page.nodes {
                if let Some(blob) = node.content {
                    blobs.push(ItemKey::Canonical(CanonicalKey::Blob(blob)).id().text());
                }
                if node.kind.is_directory() {
                    queue.push(node.id);
                }
            }
        }
    }
    blobs.sort();
    blobs.dedup();
    blobs
}

/// SYNC-026: a rename changes the display name and nothing else — every
/// identity in the renamed tree already existed in the old one.
#[test]
fn rename_preserves_every_identity() {
    let before = build(Vec::new(), vec![spec_chat()]).unwrap();
    let mut renamed = spec_chat();
    renamed.title = "Renamed Chat".to_string();
    let after = build(Vec::new(), vec![renamed]).unwrap();

    let ids_before = all_ids(&before);
    let ids_after = all_ids(&after);
    assert_eq!(ids_before, ids_after, "rename must not mint identities");
    assert_ne!(
        walk_lines(&before, page(64)).unwrap(),
        walk_lines(&after, page(64)).unwrap(),
        "the display name did change"
    );
}

fn all_ids(tree: &TreeProjection) -> Vec<String> {
    let mut ids = vec![tree.root_id().text()];
    let mut queue = vec![tree.root_id()];
    while let Some(id) = queue.pop() {
        if let Ok(page) = tree.children(&id, None, page(64)) {
            for node in page.nodes {
                ids.push(node.id.text());
                if node.kind.is_directory() {
                    queue.push(node.id);
                }
            }
        }
    }
    ids.sort();
    ids
}

/// DEC-007/SYNC-060: nothing in the tree advertises a write capability;
/// directories enumerate, files read, and node/parent links are consistent.
#[test]
fn capabilities_are_read_only_and_links_resolve() {
    let mut chat = spec_chat();
    chat.memberships = vec![ChatListKind::Main, ChatListKind::Archive];
    let tree = build(Vec::new(), vec![chat]).unwrap();

    let mut queue = vec![tree.root_id()];
    let mut seen = 0;
    while let Some(id) = queue.pop() {
        let node = tree.node(&id).unwrap();
        seen += 1;
        let caps = node.capabilities;
        assert!(!caps.write_content, "{}", node.display_name);
        assert!(!caps.rename, "{}", node.display_name);
        assert!(!caps.relocate, "{}", node.display_name);
        assert!(!caps.delete, "{}", node.display_name);
        assert_eq!(caps.enumerate_children, node.kind.is_directory());
        assert_eq!(caps.read_content, !node.kind.is_directory());

        // Parent link resolves and is a directory containing this node.
        if let Some(parent) = &node.parent {
            let parent_node = tree.node(parent).unwrap();
            assert!(parent_node.kind.is_directory());
        } else {
            assert_eq!(node.kind, NodeKind::Root);
        }

        if node.kind.is_directory() {
            let page = tree.children(&id, None, page(64)).unwrap();
            for child in page.nodes {
                assert_eq!(child.parent.as_ref(), Some(&id), "{}", child.display_name);
                queue.push(child.id);
            }
        }
    }
    assert!(seen > 10, "walk covered the tree, saw {seen}");
}

/// Identities that name records rather than tree positions do not resolve:
/// unwrapped canonical chats, non-member appearances, foreign scopes.
#[test]
fn non_positions_do_not_resolve() {
    let tree = build(Vec::new(), vec![spec_chat()]).unwrap();
    let scope = scope();
    let chat_key = ChatKey {
        scope,
        chat_id: ChatId(100),
    };

    // The canonical chat is a record, not a position.
    assert!(
        tree.node(&ItemKey::Canonical(CanonicalKey::Chat(chat_key)).id())
            .is_none()
    );
    // The chat is not a member of Archive.
    assert!(
        tree.node(
            &ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Archive,
                item: CanonicalKey::Chat(chat_key),
            })
            .id(),
        )
        .is_none()
    );
    // A view that does not exist.
    assert!(
        tree.node(
            &ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Folder(FolderId(9)),
                item: CanonicalKey::Chat(chat_key),
            })
            .id(),
        )
        .is_none()
    );
    // Chat lists never appear wrapped in a view.
    assert!(
        tree.node(
            &ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Main,
                item: CanonicalKey::Account(scope.account),
            })
            .id(),
        )
        .is_none()
    );
}

/// SYNC-003 page boundaries: chunked pages concatenate to the full listing,
/// and a boundary that is not a child of the parent fails loudly.
#[test]
fn page_boundaries_chain_without_gaps_or_repeats() {
    let chats: Vec<ChatRecord> = (0..5)
        .map(|n| ChatRecord {
            chat_id: ChatId(100 + n),
            title: format!("Chat {n}"),
            username: None,
            memberships: vec![ChatListKind::Main],
            message_months: Vec::new(),
            attachments: Vec::new(),
        })
        .collect();
    let tree = build(Vec::new(), chats).unwrap();

    let main = tree
        .children(&tree.root_id(), None, page(1))
        .unwrap()
        .nodes
        .remove(0);

    let full: Vec<String> = tree
        .children(&main.id, None, page(64))
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| node.id.text())
        .collect();
    // The five chats, plus the list root's own order.json (POL-1).
    assert_eq!(full.len(), 6);

    for size in 1..=3usize {
        let mut chunked = Vec::new();
        let mut after: Option<ItemId> = None;
        let mut hops = 0;
        loop {
            let page = tree.children(&main.id, after.as_ref(), page(size)).unwrap();
            assert!(page.nodes.len() <= size);
            chunked.extend(page.nodes.iter().map(|node| node.id.text()));
            hops += 1;
            assert!(hops <= 8, "pagination must terminate");
            match page.next {
                Some(boundary) => after = Some(boundary),
                None => break,
            }
        }
        assert_eq!(chunked, full, "page size {size}");
    }

    // A boundary from a different parent is foreign, not silently mapped.
    assert_eq!(
        tree.children(&main.id, Some(&tree.root_id()), page(2)),
        Err(ChildrenError::ForeignPageBoundary)
    );
}

/// Files reject enumeration; unknown identities reject with UnknownParent.
#[test]
fn children_errors_are_typed() {
    let tree = build(Vec::new(), vec![spec_chat()]).unwrap();
    let scope = scope();
    let chat_key = ChatKey {
        scope,
        chat_id: ChatId(100),
    };
    let chat_id = ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::Chat(chat_key),
    })
    .id();
    let chat_json = tree
        .children(&chat_id, None, page(1))
        .unwrap()
        .nodes
        .remove(0);
    assert_eq!(chat_json.display_name, "chat.json");
    assert_eq!(
        tree.children(&chat_json.id, None, page(1)),
        Err(ChildrenError::NotADirectory)
    );
    assert_eq!(
        tree.children(
            &ItemKey::Canonical(CanonicalKey::Chat(chat_key)).id(),
            None,
            page(1),
        ),
        Err(ChildrenError::UnknownParent)
    );
}

/// Every input-contract violation fails with its own error.
#[test]
fn input_violations_fail_loudly() {
    let folder = FolderRecord {
        folder_id: FolderId(7),
        title: "Work".to_string(),
    };
    assert_eq!(
        build(vec![folder.clone(), folder.clone()], Vec::new()).err(),
        Some(TreeInputError::DuplicateFolder {
            folder: FolderId(7)
        })
    );
    assert_eq!(
        build(Vec::new(), vec![spec_chat(), spec_chat()]).err(),
        Some(TreeInputError::DuplicateChat { chat: ChatId(100) })
    );

    let mut dangling = spec_chat();
    dangling.memberships = vec![ChatListKind::Folder(FolderId(9))];
    assert_eq!(
        build(Vec::new(), vec![dangling]).err(),
        Some(TreeInputError::UnknownFolderMembership {
            chat: ChatId(100),
            folder: FolderId(9),
        })
    );

    for month in [0u8, 13] {
        let mut invalid = spec_chat();
        invalid.message_months = vec![MonthStamp { year: 2026, month }];
        assert_eq!(
            build(Vec::new(), vec![invalid]).err(),
            Some(TreeInputError::InvalidMonth {
                chat: ChatId(100),
                stamp: MonthStamp { year: 2026, month },
            })
        );
    }

    let mut invalid_attachment_month = spec_chat();
    invalid_attachment_month.attachments = vec![AttachmentRecord {
        month: MonthStamp {
            year: 2026,
            month: 13,
        },
        ..photo(500)
    }];
    assert_eq!(
        build(Vec::new(), vec![invalid_attachment_month]).err(),
        Some(TreeInputError::InvalidMonth {
            chat: ChatId(100),
            stamp: MonthStamp {
                year: 2026,
                month: 13,
            },
        })
    );

    let mut duplicated = spec_chat();
    duplicated.attachments = vec![photo(500), photo(500)];
    assert_eq!(
        build(Vec::new(), vec![duplicated]).err(),
        Some(TreeInputError::DuplicateAttachment {
            chat: ChatId(100),
            message: MessageId(500),
            index: AttachmentIndex(0),
        })
    );
}

// ---------------------------------------------------------------------------
// The ordering document at each list root (TASK-260715-1jmsdp; POL-1)
// ---------------------------------------------------------------------------

fn order_doc_key(kind: ChatListKind) -> CanonicalKey {
    CanonicalKey::OrderDoc(gramdrive_model::identity::OrderDocKey {
        list: gramdrive_model::identity::ChatListKey {
            scope: scope(),
            kind,
        },
        schema_family: schemas().order_json,
    })
}

/// Every list root — Main, Archive, and each custom folder — publishes one
/// `order.json`, and the folder catalog (which is not a list) does not.
#[test]
fn every_list_root_publishes_an_order_document() {
    let mut chat = spec_chat();
    chat.memberships = vec![ChatListKind::Main, ChatListKind::Folder(FolderId(7))];
    let tree = build(
        vec![FolderRecord {
            folder_id: FolderId(7),
            title: "Work".to_string(),
        }],
        vec![chat],
    )
    .unwrap();

    for kind in [
        ChatListKind::Main,
        ChatListKind::Archive,
        ChatListKind::Folder(FolderId(7)),
    ] {
        let list = ItemKey::Canonical(CanonicalKey::ChatList(
            gramdrive_model::identity::ChatListKey {
                scope: scope(),
                kind,
            },
        ))
        .id();
        let first = tree
            .children(&list, None, page(64))
            .unwrap()
            .nodes
            .remove(0);
        assert_eq!(first.display_name, "order.json", "list {kind:?}");
        assert_eq!(first.kind, NodeKind::GeneratedDoc);
        assert_eq!(first.parent.as_ref(), Some(&list));
        // Read-only, like every v1 item (DEC-007).
        assert!(!first.capabilities.write_content);
        assert!(!first.capabilities.rename);
        assert!(first.capabilities.read_content);
        assert!(!first.capabilities.enumerate_children);
    }

    // The folder catalog groups views; it is not one, so it has no order.
    let catalog = ItemKey::Canonical(CanonicalKey::FolderCatalog(
        gramdrive_model::identity::FolderCatalogKey { scope: scope() },
    ))
    .id();
    let names: Vec<String> = tree
        .children(&catalog, None, page(64))
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| node.display_name)
        .collect();
    assert_eq!(names, ["Work"]);
}

/// The ordering document resolves by identity, and its identity does not
/// depend on the order it records — there is nothing in the key to change.
#[test]
fn the_order_document_resolves_and_is_canonical() {
    let tree = build(Vec::new(), vec![spec_chat()]).unwrap();
    let id = ItemKey::Canonical(order_doc_key(ChatListKind::Main)).id();
    let node = tree.node(&id).expect("order.json resolves");
    assert_eq!(node.canonical, order_doc_key(ChatListKind::Main));
    assert_eq!(node.display_name, "order.json");
    // Canonical, not an appearance: one document per list, not per view of a
    // chat. Enumerating it as a child yields the same identity.
    assert_eq!(node.id, id);
}

/// A document at a schema family this tree does not publish is not a node —
/// the same discipline the chat documents apply (DOM-023).
#[test]
fn an_order_document_of_a_foreign_schema_family_is_not_a_node() {
    let tree = build(Vec::new(), Vec::new()).unwrap();
    let foreign = ItemKey::Canonical(CanonicalKey::OrderDoc(
        gramdrive_model::identity::OrderDocKey {
            list: gramdrive_model::identity::ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            schema_family: SchemaFamily(999),
        },
    ))
    .id();
    assert!(tree.node(&foreign).is_none());
}

/// A document of a folder view that does not exist is not a node either.
#[test]
fn an_order_document_of_an_unknown_folder_is_not_a_node() {
    let tree = build(Vec::new(), Vec::new()).unwrap();
    let ghost = ItemKey::Canonical(order_doc_key(ChatListKind::Folder(FolderId(404)))).id();
    assert!(tree.node(&ghost).is_none());
}

/// order.json is a file: it has no children, and asking for them says so.
#[test]
fn the_order_document_is_not_a_directory() {
    let tree = build(Vec::new(), Vec::new()).unwrap();
    let id = ItemKey::Canonical(order_doc_key(ChatListKind::Main)).id();
    assert_eq!(
        tree.children(&id, None, page(4)),
        Err(ChildrenError::NotADirectory)
    );
}
