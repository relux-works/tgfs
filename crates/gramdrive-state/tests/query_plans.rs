//! EXPLAIN evidence on the synthetic large account (TASK-260715-1ceq7h AC):
//! every query path the schema exists to serve runs against a database of
//! thousands of chats and 100k+ messages, and its plan must go through an
//! index — a bare table scan on any of them is a failed acceptance
//! criterion, not a slow day.
//!
//! The fixture is `gramdrive_testkit::synthetic` — the same generator later
//! performance tasks reuse — loaded through the real schema with all
//! constraints on. Set `GRAMDRIVE_PLAN_EVIDENCE=/path/file.md` to write the
//! captured plans as a reviewable artifact.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the fixture-loading helpers below are neither:
// they sit at module level in an integration-test binary. The rationale still
// applies in full — this file links into no product artifact — so the
// exemption is restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use common::{ACCOUNT_ID, NAMESPACE, account_root_id, appearance_id};
use gramdrive_state::StateStore;
use gramdrive_state::model::identity::{
    CanonicalKey, ChatListKind, DocFormat, DocPartition, ItemId, MonthDirKey,
};
use gramdrive_testkit::synthetic::{
    self, SyntheticAccount, SyntheticChat, SyntheticSpec, partition_of,
};
use rusqlite::{Connection, Transaction, params};

/// One required query path: what it serves, and the SQL whose plan must be
/// index-driven.
struct RequiredQuery {
    name: &'static str,
    serves: &'static str,
    sql: &'static str,
}

const REQUIRED_QUERIES: &[RequiredQuery] = &[
    RequiredQuery {
        name: "item_by_id",
        serves: "provider metadata lookup by stable ItemId (DOM-024)",
        sql: "SELECT kind, display_name, logical_size FROM items WHERE item_id = ?1",
    },
    RequiredQuery {
        name: "children_page",
        serves: "paged enumeration anchored at the last returned child (SYNC-003)",
        sql: "SELECT item_id, safe_name FROM items
              WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL AND item_id > ?2
              ORDER BY item_id LIMIT 200",
    },
    RequiredQuery {
        name: "child_by_name",
        serves: "path resolution one component at a time (DOM-005)",
        sql: "SELECT item_id FROM items
              WHERE parent_item_id = ?1 AND safe_name = ?2 AND deleted_at_ms IS NULL",
    },
    RequiredQuery {
        name: "appearances_of_canonical",
        serves: "propagating a canonical change to every view (SYNC-026)",
        sql: "SELECT item_id, view_kind FROM items WHERE canonical_item_id = ?1",
    },
    RequiredQuery {
        name: "chat_list_order",
        serves: "order.json regeneration and app-UI order (POL-1)",
        sql: "SELECT chat_id, pinned, sort_order FROM chat_list_entries
              WHERE account_id = ?1 AND namespace_version = ?2
                AND list_kind = ?3 AND folder_id = ?4
              ORDER BY pinned DESC, sort_order DESC",
    },
    RequiredQuery {
        name: "chat_messages_by_id_range",
        serves: "resumable, idempotent history traversal (SYNC-021)",
        sql: "SELECT message_id, sent_at_ms FROM messages
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id > ?4
              ORDER BY message_id LIMIT 500",
    },
    RequiredQuery {
        name: "chat_messages_by_time_window",
        serves: "month/year partition rendering (SYNC-031)",
        sql: "SELECT message_id FROM messages
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND sent_at_ms >= ?4 AND sent_at_ms < ?5",
    },
    RequiredQuery {
        name: "chat_event_tail",
        serves: "render catch-up from a watermark (SYNC-022, SYNC-024)",
        sql: "SELECT event_seq, event_kind FROM message_events
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND event_seq > ?4
              ORDER BY event_seq",
    },
    RequiredQuery {
        name: "affected_render_instants",
        serves: "month-only invalidation from an event watermark interval (SYNC-024)",
        sql: "SELECT m.sent_at_ms
              FROM message_events e
              JOIN messages m
                ON m.account_id = e.account_id
               AND m.namespace_version = e.namespace_version
               AND m.chat_id = e.chat_id
               AND m.message_id = e.message_id
              WHERE e.account_id = ?1 AND e.namespace_version = ?2 AND e.chat_id = ?3
                AND e.event_seq > ?4 AND e.event_seq <= ?5",
    },
    RequiredQuery {
        name: "message_event_history",
        serves: "Audit-mode revision history of one message (POL-3)",
        sql: "SELECT event_seq, event_kind FROM message_events
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id = ?4
              ORDER BY event_seq",
    },
    RequiredQuery {
        name: "attachments_of_message",
        serves: "attachment listing while rendering a message",
        sql: "SELECT attachment_index, original_name, logical_size FROM attachments
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id = ?4",
    },
    RequiredQuery {
        name: "attachments_by_blob",
        serves: "who still references these bytes (SYNC-052)",
        sql: "SELECT chat_id, message_id, attachment_index FROM attachments
              WHERE account_id = ?1 AND blob_hash_algo = 'sha256' AND blob_hash = ?2",
    },
    RequiredQuery {
        name: "transfer_queue_head",
        serves: "scheduler picking the next hydration (SYNC-040)",
        sql: "SELECT transfer_id, item_id FROM transfers
              WHERE state = 'queued'
              ORDER BY priority DESC, transfer_id LIMIT 1",
    },
    RequiredQuery {
        name: "live_transfer_for_item_version",
        serves: "coalescing concurrent requests (SYNC-046)",
        sql: "SELECT transfer_id, state FROM transfers
              WHERE item_id = ?1 AND content_version = ?2",
    },
    RequiredQuery {
        name: "eviction_candidates",
        serves: "LRU eviction over eligible content only (POL-2, SYNC-051/052)",
        sql: "SELECT item_id, size FROM cache_entries
              WHERE pinned = 0 AND verification = 'verified'
              ORDER BY last_access_at_ms LIMIT 64",
    },
    RequiredQuery {
        name: "cache_accounting",
        serves: "quota accounting by category (SYNC-050)",
        sql: "SELECT kind, sum(size) FROM cache_entries
              WHERE account_id = ?1 GROUP BY kind",
    },
    RequiredQuery {
        name: "generated_materialization_reference_claim",
        serves: "generated publication reclamation without starving foreground hydration",
        sql: "SELECT EXISTS (
              SELECT 1 FROM cache_entries WHERE materialization_ref = ?1)",
    },
    RequiredQuery {
        name: "cursor_lookup",
        serves: "restoring the durable change-feed position (SYNC-004, SYNC-022)",
        sql: "SELECT cursor_text FROM change_cursors WHERE account_id = ?1 AND stream = ?2",
    },
    RequiredQuery {
        name: "backfill_backlog",
        serves: "bounded runnable listed chats including never-anchored eligible rows (SYNC-021)",
        sql: "SELECT s.chat_id
              FROM chat_sync_state s
              JOIN chats c
                ON c.account_id = s.account_id
               AND c.namespace_version = s.namespace_version
               AND c.chat_id = s.chat_id
              LEFT JOIN chat_content_progress p
                ON p.account_id = s.account_id
               AND p.namespace_version = s.namespace_version
               AND p.chat_id = s.chat_id
              WHERE s.account_id = ?1 AND s.namespace_version = ?2
                AND s.history_complete = 0
                AND c.deleted_at_ms IS NULL
                AND c.is_protected = 0
                AND EXISTS (
                    SELECT 1 FROM chat_list_entries e
                    WHERE e.account_id = s.account_id
                      AND e.namespace_version = s.namespace_version
                      AND e.chat_id = s.chat_id
                )
                AND (p.chat_id IS NULL
                     OR p.phase IN ('pending', 'syncing', 'cancelled')
                     OR (p.phase = 'degraded'
                         AND (p.retry_at_ms IS NULL OR p.retry_at_ms <= ?4)))
              ORDER BY s.last_backfill_at_ms, s.chat_id LIMIT 32",
    },
    RequiredQuery {
        name: "backfill_control_lookup",
        serves: "the scheduler's durable pause/pacing state for one scope (POL-8, NFR-033)",
        sql: "SELECT paused, next_request_at_ms, flood_wait_until_ms, updated_at_ms
              FROM backfill_control
              WHERE account_id = ?1 AND namespace_version = ?2",
    },
    RequiredQuery {
        name: "dirty_render_docs",
        serves: "the re-render worklist (SYNC-024)",
        sql: "SELECT item_id FROM render_state WHERE dirty = 1",
    },
    RequiredQuery {
        name: "monthly_render_catalog",
        serves: "bounded Markdown/NDJSON appearance catalog for one direct month (SYNC-033)",
        sql: "SELECT child.item_id
              FROM items AS month
              JOIN items AS child ON child.parent_item_id = month.item_id
              WHERE month.canonical_item_id = ?1
                AND month.kind = 'month_dir' AND month.deleted_at_ms IS NULL
                AND child.kind = 'generated_doc' AND child.deleted_at_ms IS NULL",
    },
    RequiredQuery {
        name: "chat_render_catalog",
        serves: "bounded .chat.json appearance catalog for one chat (SYNC-033)",
        sql: "SELECT child.item_id
              FROM items AS chat
              JOIN items AS child ON child.parent_item_id = chat.item_id
              WHERE chat.canonical_item_id = ?1
                AND chat.kind = 'chat' AND chat.deleted_at_ms IS NULL
                AND child.kind = 'generated_doc' AND child.deleted_at_ms IS NULL",
    },
    RequiredQuery {
        name: "generated_documents_of_chat",
        serves: "privacy transitions over one chat without scanning or sorting the account catalog",
        sql: "SELECT document.item_id
              FROM items AS chat
              JOIN items AS document ON document.parent_item_id = chat.item_id
              WHERE chat.canonical_item_id = ?1
                AND chat.kind = 'chat' AND chat.deleted_at_ms IS NULL
                AND document.kind = 'generated_doc' AND document.deleted_at_ms IS NULL
              UNION ALL
              SELECT document.item_id
              FROM items AS chat
              JOIN items AS month ON month.parent_item_id = chat.item_id
              JOIN items AS document ON document.parent_item_id = month.item_id
              WHERE chat.canonical_item_id = ?1
                AND chat.kind = 'chat' AND chat.deleted_at_ms IS NULL
                AND month.kind = 'month_dir' AND month.deleted_at_ms IS NULL
                AND document.kind = 'generated_doc' AND document.deleted_at_ms IS NULL",
    },
];

#[test]
fn required_queries_avoid_full_scans_on_the_large_account() {
    let account = synthetic::generate(&SyntheticSpec::large_account());
    let mut store = StateStore::open_in_memory().expect("open");
    let stats = load_account(&mut store, &account, 6_609);

    // The planner should see real table shapes, as a deployed database
    // would after PRAGMA optimize.
    store
        .connection()
        .execute_batch("ANALYZE;")
        .expect("analyze");

    let mut evidence = String::new();
    let _ = writeln!(
        evidence,
        "# EXPLAIN evidence — synthetic large account\n\n\
         Fixture: `gramdrive_testkit::synthetic::SyntheticSpec::large_account()` \
         (seed {:#x}).\n\n\
         | rows | count |\n|---|---|\n\
         | chats | {} |\n| messages | {} |\n| message_events | {} |\n\
         | attachments | {} |\n| items | {} |\n| transfers | {} |\n\
         | cache_entries | {} |\n| render_state | {} |\n\
         | active backfills | {} |\n",
        SyntheticSpec::large_account().seed,
        stats.chats,
        stats.messages,
        stats.events,
        stats.attachments,
        stats.items,
        stats.transfers,
        stats.cache_entries,
        stats.render_state,
        stats.active_backfills,
    );

    assert!(stats.chats >= 2_000, "AC: thousands of chats");
    assert!(stats.messages >= 100_000, "AC: 100k+ messages");
    assert!(
        stats.active_backfills >= 6_609,
        "AC: preserved-profile-scale active backfills"
    );

    let mut failures = Vec::new();
    for query in REQUIRED_QUERIES {
        let plan = explain(store.connection(), query.sql);
        let _ = writeln!(
            evidence,
            "\n## {}\n\nServes: {}.\n\n```sql\n{}\n```\n\n```text\n{}\n```",
            query.name,
            query.serves,
            query.sql,
            plan.join("\n")
        );
        for line in &plan {
            // SEARCH is an index probe. A SCAN is acceptable only when it
            // walks an index (partial or covering) rather than the table;
            // a temp b-tree means an ORDER BY the schema failed to serve.
            let scan_without_index =
                line.starts_with("SCAN") && !line.contains("USING") && line != "SCAN CONSTANT ROW";
            let temp_btree = line.contains("USE TEMP B-TREE");
            if scan_without_index || temp_btree {
                failures.push(format!("{}: {line}", query.name));
            }
        }
    }

    if let Ok(path) = std::env::var("GRAMDRIVE_PLAN_EVIDENCE") {
        std::fs::write(&path, &evidence).expect("write evidence file");
    }

    assert!(
        failures.is_empty(),
        "queries fell back to full scans or temp sorts:\n{}",
        failures.join("\n")
    );
}

#[test]
fn required_queries_return_real_rows_from_the_fixture() {
    // Plans alone can lie about relevance: an index-driven query over the
    // wrong predicate returns nothing forever. Prove each hot path also
    // *finds* what the fixture put there. The small spec keeps this fast;
    // shape, not volume, is under test here.
    let account = synthetic::generate(&SyntheticSpec::small());
    let mut store = StateStore::open_in_memory().expect("open");
    load_account(&mut store, &account, 0);
    let conn = store.connection();

    // Root enumeration sees the four fixed roots (Main, Archive, Stories,
    // and the folder catalog).
    let children: i64 = conn
        .query_row(
            "SELECT count(*) FROM items WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL",
            params![account_root_id().as_bytes()],
            |r| r.get(0),
        )
        .expect("children");
    assert_eq!(children, 4);

    // The busiest chat's messages come back id-ordered from the PK path.
    let busiest = account
        .chats
        .iter()
        .max_by_key(|c| c.messages.len())
        .expect("chats exist");
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT message_id FROM messages
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
               AND message_id > 0 ORDER BY message_id LIMIT 500",
        )
        .expect("prepare")
        .query_map(params![ACCOUNT_ID, NAMESPACE, busiest.chat_id.0], |r| {
            r.get(0)
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(!ids.is_empty());
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "id-ordered");

    // The chat list order query returns exactly the main-list membership.
    let main_members = account
        .chats
        .iter()
        .filter(|c| {
            c.list_entries
                .iter()
                .any(|e| matches!(e.list, ChatListKind::Main))
        })
        .count() as i64;
    let listed: i64 = conn
        .query_row(
            "SELECT count(*) FROM chat_list_entries
             WHERE account_id = ?1 AND namespace_version = ?2
               AND list_kind = 'main' AND folder_id = 0",
            params![ACCOUNT_ID, NAMESPACE],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(listed, main_members);

    // Engine state derived from the fixture is present and queryable.
    let evictable: i64 = conn
        .query_row(
            "SELECT count(*) FROM cache_entries WHERE pinned = 0 AND verification = 'verified'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert!(evictable > 0, "eviction candidates exist");
    let queued: i64 = conn
        .query_row(
            "SELECT count(*) FROM transfers WHERE state = 'queued'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert!(queued > 0, "queued transfers exist");
    let dirty: i64 = conn
        .query_row(
            "SELECT count(*) FROM render_state WHERE dirty = 1",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert!(dirty > 0, "dirty documents exist");

    // The repository catalog has the same direct .chat.json appearances the
    // projection created, and keeps their previous stable ItemId order.
    let target = account
        .chats
        .iter()
        .max_by_key(|chat| chat.list_entries.len())
        .expect("chats exist");
    let mut expected: Vec<_> = target
        .list_entries
        .iter()
        .map(|entry| {
            appearance_id(
                entry.list,
                common::doc_key(target.chat_id.0, DocPartition::Chat, DocFormat::Json),
            )
        })
        .collect();
    expected.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let read = store.read_txn().expect("read");
    let catalog = read
        .chat_render_catalog(common::chat_key(target.chat_id.0))
        .expect("chat catalog");
    let actual: Vec<_> = catalog.into_iter().map(|entry| entry.item).collect();
    assert_eq!(
        actual, expected,
        "catalog entries and ItemId order are stable"
    );

    let months: BTreeSet<_> = target
        .messages
        .iter()
        .map(|message| partition_of(message.sent_at_ms))
        .collect();
    let mut expected_all = Vec::new();
    for entry in &target.list_entries {
        expected_all.push(appearance_id(
            entry.list,
            common::doc_key(target.chat_id.0, DocPartition::Chat, DocFormat::Json),
        ));
        for &(year, month) in &months {
            for format in [DocFormat::Markdown, DocFormat::Ndjson] {
                expected_all.push(appearance_id(
                    entry.list,
                    common::doc_key(
                        target.chat_id.0,
                        DocPartition::Month { year, month },
                        format,
                    ),
                ));
            }
        }
    }
    expected_all.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let actual_all = read
        .generated_document_items_of_chat(&common::chat_key(target.chat_id.0))
        .expect("all generated documents of chat");
    assert_eq!(
        actual_all, expected_all,
        "bounded chat-subtree lookup preserves every appearance and stable ItemId order"
    );
}

fn explain(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare explain");
    // The plan is computed from the statement shape; unbound parameters
    // only need to exist, so bind a NULL for each placeholder.
    let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
    let rows = stmt
        .query_map(rusqlite::params_from_iter(nulls), |row| {
            row.get::<_, String>(3)
        })
        .expect("run explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("plan rows");
    assert!(!rows.is_empty(), "explain produced no plan for: {sql}");
    rows
}

/// Row counts after loading, for the evidence report and the AC assertions.
struct LoadStats {
    chats: i64,
    messages: i64,
    events: i64,
    attachments: i64,
    items: i64,
    transfers: i64,
    cache_entries: i64,
    render_state: i64,
    active_backfills: i64,
}

/// Maps the synthetic account onto the schema: canonical facts, the event
/// log with its `messages` projection, the full provider projection for
/// every list membership (each chat's subtree appears once per view,
/// DOM-022), and engine state (blobs, cache, pins, transfers, render
/// state, cursors, sync windows) derived deterministically from the same
/// data.
fn load_account(
    store: &mut StateStore,
    account: &SyntheticAccount,
    additional_active_backfills: usize,
) -> LoadStats {
    let tx = store.connection_mut().transaction().expect("begin");

    tx.execute(
        "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                               namespace_version, created_at_ms, updated_at_ms)
         VALUES (?1, 'local_tdlib', ?2, 'authorized', ?3, 0, 0)",
        params![ACCOUNT_ID, account.display_name, NAMESPACE],
    )
    .expect("account");

    insert_structure_items(&tx, account);

    let mut attachment_counter: u64 = 0;
    for chat in &account.chats {
        insert_chat_facts(&tx, chat);
        insert_chat_projection(&tx, chat, &mut attachment_counter);
    }

    // The installed regression had 6,609 simultaneously runnable backfills.
    // These metadata-only listed chats exercise that scheduler cardinality
    // without inflating the already representative 100k-message payload.
    for offset in 0..additional_active_backfills {
        let chat_id = 9_000_000_i64 + i64::try_from(offset).expect("backfill offset");
        tx.execute(
            "INSERT INTO chats (
                 account_id, namespace_version, chat_id, chat_type, title,
                 is_protected, metadata_version, last_update_at_ms
             ) VALUES (?1, ?2, ?3, 'private', 'synthetic backfill', 0, 'm1', 0)",
            params![ACCOUNT_ID, NAMESPACE, chat_id],
        )
        .expect("saturated backfill chat");
        tx.execute(
            "INSERT INTO chat_list_entries (
                 account_id, namespace_version, list_kind, folder_id,
                 chat_id, sort_order, pinned
             ) VALUES (?1, ?2, 'main', 0, ?3, ?4, 0)",
            params![ACCOUNT_ID, NAMESPACE, chat_id, -chat_id],
        )
        .expect("saturated backfill membership");
    }

    tx.execute(
        "INSERT INTO change_cursors (account_id, namespace_version, stream, cursor_text,
                                     updated_at_ms)
         VALUES (?1, ?2, 'drive', 'gdc-synthetic-baseline', 0)",
        params![ACCOUNT_ID, NAMESPACE],
    )
    .expect("cursor");

    tx.commit().expect("commit");

    let count = |table: &str| -> i64 {
        store
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count")
    };
    LoadStats {
        chats: count("chats"),
        messages: count("messages"),
        events: count("message_events"),
        attachments: count("attachments"),
        items: count("items"),
        transfers: count("transfers"),
        cache_entries: count("cache_entries"),
        render_state: count("render_state"),
        active_backfills: store
            .connection()
            .query_row(
                "SELECT count(*) FROM chat_sync_state WHERE history_complete = 0",
                [],
                |row| row.get(0),
            )
            .expect("active backfill count"),
    }
}

/// The fixed structural nodes: account root, Main and Archive roots, the
/// folder catalog with one root per declared folder, and an order.json
/// under every list root.
fn insert_structure_items(tx: &Transaction<'_>, account: &SyntheticAccount) {
    let root = account_root_id();
    insert_dir(tx, &root, None, "account", &account.display_name);

    let main = common::chat_list_id(ChatListKind::Main);
    let archive = common::chat_list_id(ChatListKind::Archive);
    let stories = common::chat_list_id(ChatListKind::Stories);
    insert_dir(tx, &main, Some(&root), "chat_list", "Main");
    insert_dir(tx, &archive, Some(&root), "chat_list", "Archive");
    insert_dir(tx, &stories, Some(&root), "chat_list", "Stories");
    insert_order_doc(tx, &main, ChatListKind::Main);
    insert_order_doc(tx, &archive, ChatListKind::Archive);
    insert_order_doc(tx, &stories, ChatListKind::Stories);

    let catalog = gramdrive_state::model::identity::ItemKey::Canonical(
        CanonicalKey::FolderCatalog(gramdrive_state::model::identity::FolderCatalogKey {
            scope: common::scope(),
        }),
    )
    .id();
    insert_dir(
        tx,
        &catalog,
        Some(&root),
        "folder_catalog",
        "Telegram Folders",
    );

    for folder in &account.folders {
        let list = ChatListKind::Folder(folder.folder_id);
        let folder_root = common::chat_list_id(list);
        insert_dir(tx, &folder_root, Some(&catalog), "chat_list", &folder.title);
        insert_order_doc(tx, &folder_root, list);
    }
}

fn insert_order_doc(tx: &Transaction<'_>, list_root: &ItemId, list: ChatListKind) {
    let doc =
        gramdrive_state::model::identity::ItemKey::Canonical(common::order_doc_key(list)).id();
    tx.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, mime_type,
                            metadata_version, content_version)
         VALUES (?1, ?2, ?3, 'order_doc', ?4, 'order.json', 'order.json', 0,
                 'application/json', 'm1', 'ord-1')",
        params![doc.as_bytes(), ACCOUNT_ID, NAMESPACE, list_root.as_bytes()],
    )
    .expect("order doc item");
}

fn insert_chat_facts(tx: &Transaction<'_>, chat: &SyntheticChat) {
    use gramdrive_testkit::synthetic::SyntheticChatType;
    let chat_type = match chat.chat_type {
        SyntheticChatType::Private => "private",
        SyntheticChatType::Group => "group",
        SyntheticChatType::Supergroup => "supergroup",
        SyntheticChatType::Channel => "channel",
    };
    tx.execute(
        "INSERT INTO chats (account_id, namespace_version, chat_id, chat_type, title,
                            username, is_protected, metadata_version, last_update_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'm1', 0)",
        params![
            ACCOUNT_ID,
            NAMESPACE,
            chat.chat_id.0,
            chat_type,
            chat.title,
            chat.username,
            chat.is_protected as i64,
        ],
    )
    .expect("chat");

    for entry in &chat.list_entries {
        let (list_kind, folder_id) = match entry.list {
            ChatListKind::Main => ("main", 0),
            ChatListKind::Archive => ("archive", 0),
            ChatListKind::Stories => ("stories", 0),
            ChatListKind::Folder(id) => ("folder", id.0),
        };
        tx.execute(
            "INSERT INTO chat_list_entries (account_id, namespace_version, list_kind,
                                            folder_id, chat_id, sort_order, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ACCOUNT_ID,
                NAMESPACE,
                list_kind,
                folder_id,
                chat.chat_id.0,
                entry.sort_order,
                entry.pinned as i64,
            ],
        )
        .expect("list entry");
    }

    // The event log and its projection: an 'observed' event per message,
    // 'edited' for edits, a 'deleted' tombstone where a deletion was
    // observed; messages.latest_event_seq points at the newest.
    for message in &chat.messages {
        let mut latest = append_event(
            tx,
            chat,
            message.message_id.0,
            "observed",
            message.sent_at_ms,
            true,
        );
        if let Some(edited_at) = message.edited_at_ms {
            latest = append_event(tx, chat, message.message_id.0, "edited", edited_at, true);
        }
        if message.deleted {
            latest = append_event(
                tx,
                chat,
                message.message_id.0,
                "deleted",
                message.sent_at_ms + 86_400_000,
                false,
            );
        }
        tx.execute(
            "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                                   sender_id, sent_at_ms, edited_at_ms, is_deleted,
                                   latest_event_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                ACCOUNT_ID,
                NAMESPACE,
                chat.chat_id.0,
                message.message_id.0,
                message.sender_id,
                message.sent_at_ms,
                message.edited_at_ms,
                message.deleted as i64,
                latest,
            ],
        )
        .expect("message");
    }

    // Per-chat sync window (SYNC-021), deterministic from shape alone.
    if let (Some(first), Some(last)) = (chat.messages.first(), chat.messages.last()) {
        let complete = !chat.messages.len().is_multiple_of(5);
        tx.execute(
            "INSERT OR REPLACE INTO chat_sync_state (account_id, namespace_version, chat_id,
                                          oldest_loaded_message_id, newest_loaded_message_id,
                                          history_complete, last_sync_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ACCOUNT_ID,
                NAMESPACE,
                chat.chat_id.0,
                first.message_id.0,
                last.message_id.0,
                complete as i64,
                (!chat.messages.len().is_multiple_of(3)).then_some(last.sent_at_ms),
            ],
        )
        .expect("sync state");
    }
}

fn append_event(
    tx: &Transaction<'_>,
    chat: &SyntheticChat,
    message_id: i64,
    kind: &str,
    observed_at_ms: i64,
    with_payload: bool,
) -> i64 {
    let payload = with_payload.then(|| format!("rec:{}:{message_id}:{kind}", chat.chat_id.0));
    tx.execute(
        "INSERT INTO message_events (account_id, namespace_version, chat_id, message_id,
                                     event_kind, observed_at_ms, payload_schema, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ACCOUNT_ID,
            NAMESPACE,
            chat.chat_id.0,
            message_id,
            kind,
            observed_at_ms,
            with_payload.then_some(1i64),
            payload.as_deref().map(str::as_bytes),
        ],
    )
    .expect("event");
    tx.last_insert_rowid()
}

/// One full provider subtree per list membership (DOM-022): the chat
/// directory appearance, .chat.json, direct month directories, monthly
/// documents, attachment files — plus, for the first
/// (primary) membership only, the engine state hung off those items.
fn insert_chat_projection(
    tx: &Transaction<'_>,
    chat: &SyntheticChat,
    attachment_counter: &mut u64,
) {
    let canonical_chat = common::chat_canonical_key(chat.chat_id.0);
    for (entry_index, entry) in chat.list_entries.iter().enumerate() {
        let view = entry.list;
        let primary = entry_index == 0;
        let parent_root = common::chat_list_id(view);
        let chat_dir = appearance_id(view, canonical_chat);
        insert_dir_appearance(
            tx,
            &chat_dir,
            &parent_root,
            view,
            &common::canonical_chat_id(chat.chat_id.0),
            "chat",
            &chat.title,
        );

        // .chat.json is the only generated file directly under a chat.
        let key = common::doc_key(chat.chat_id.0, DocPartition::Chat, DocFormat::Json);
        let doc = appearance_id(view, key);
        insert_doc_appearance(
            tx,
            &doc,
            &chat_dir,
            view,
            key,
            ".chat.json",
            "application/json",
            chat,
            primary,
        );

        // Direct month directories contain both bounded documents and every
        // attachment in one chronological namespace.
        let months: BTreeSet<(u16, u8)> = chat
            .messages
            .iter()
            .map(|m| partition_of(m.sent_at_ms))
            .collect();
        for &(year, month) in &months {
            let month_key = CanonicalKey::MonthDir(MonthDirKey {
                chat: common::chat_key(chat.chat_id.0),
                year,
                month,
            });
            let month_dir = appearance_id(view, month_key);
            insert_dir_appearance(
                tx,
                &month_dir,
                &chat_dir,
                view,
                &gramdrive_state::model::identity::ItemKey::Canonical(month_key).id(),
                "month_dir",
                &format!("{year:04}-{month:02}"),
            );

            for (format, name, mime) in [
                (DocFormat::Markdown, "Messages.md", "text/markdown"),
                (DocFormat::Ndjson, "Messages.ndjson", "application/x-ndjson"),
            ] {
                let key =
                    common::doc_key(chat.chat_id.0, DocPartition::Month { year, month }, format);
                let doc = appearance_id(view, key);
                insert_doc_appearance(tx, &doc, &month_dir, view, key, name, mime, chat, primary);
            }
        }

        for message in &chat.messages {
            let (year, month) = partition_of(message.sent_at_ms);
            let month_dir = appearance_id(
                view,
                CanonicalKey::MonthDir(MonthDirKey {
                    chat: common::chat_key(chat.chat_id.0),
                    year,
                    month,
                }),
            );
            for attachment in &message.attachments {
                insert_attachment(
                    tx,
                    chat,
                    message.message_id.0,
                    message.sent_at_ms,
                    message.deleted,
                    attachment,
                    view,
                    &month_dir,
                    primary,
                    attachment_counter,
                );
            }
        }
    }
}

fn insert_dir(
    tx: &Transaction<'_>,
    item: &ItemId,
    parent: Option<&ItemId>,
    kind: &str,
    name: &str,
) {
    tx.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 'm1')",
        params![
            item.as_bytes(),
            ACCOUNT_ID,
            NAMESPACE,
            kind,
            parent.map(ItemId::as_bytes),
            name,
            name,
        ],
    )
    .expect("dir item");
}

fn view_columns(view: ChatListKind) -> (&'static str, Option<i32>) {
    match view {
        ChatListKind::Main => ("main", None),
        ChatListKind::Archive => ("archive", None),
        ChatListKind::Stories => ("stories", None),
        ChatListKind::Folder(id) => ("folder", Some(id.0)),
    }
}

fn insert_dir_appearance(
    tx: &Transaction<'_>,
    item: &ItemId,
    parent: &ItemId,
    view: ChatListKind,
    canonical: &ItemId,
    kind: &str,
    name: &str,
) {
    let (view_kind, folder_id) = view_columns(view);
    tx.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, view_kind, view_folder_id, display_name,
                            safe_name, is_directory, metadata_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, 'm1')",
        params![
            item.as_bytes(),
            ACCOUNT_ID,
            NAMESPACE,
            kind,
            parent.as_bytes(),
            canonical.as_bytes(),
            view_kind,
            folder_id,
            name,
            name,
        ],
    )
    .expect("dir appearance");
}

#[expect(clippy::too_many_arguments, reason = "test fixture plumbing")]
fn insert_doc_appearance(
    tx: &Transaction<'_>,
    item: &ItemId,
    parent: &ItemId,
    view: ChatListKind,
    canonical: CanonicalKey,
    name: &str,
    mime: &str,
    chat: &SyntheticChat,
    primary: bool,
) {
    let (view_kind, folder_id) = view_columns(view);
    let canonical_id = gramdrive_state::model::identity::ItemKey::Canonical(canonical).id();
    tx.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, view_kind, view_folder_id, display_name,
                            safe_name, is_directory, mime_type, metadata_version,
                            content_version)
         VALUES (?1, ?2, ?3, 'generated_doc', ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, 'm1', 'rv1')",
        params![
            item.as_bytes(),
            ACCOUNT_ID,
            NAMESPACE,
            parent.as_bytes(),
            canonical_id.as_bytes(),
            view_kind,
            folder_id,
            name,
            name,
            mime,
        ],
    )
    .expect("doc appearance");

    // Render state for the primary view's documents; every 11th chat's
    // documents are pending re-render.
    if primary {
        let dirty = chat.chat_id.0.rem_euclid(11) == 0;
        tx.execute(
            "INSERT INTO render_state (item_id, renderer_version, schema_version,
                                       input_watermark_seq, content_version, dirty,
                                       rendered_at_ms)
             VALUES (?1, 1, 1, 0, 'rv1', ?2, 0)",
            params![item.as_bytes(), dirty as i64],
        )
        .expect("render state");
    }
}

#[expect(clippy::too_many_arguments, reason = "test fixture plumbing")]
fn insert_attachment(
    tx: &Transaction<'_>,
    chat: &SyntheticChat,
    message_id: i64,
    sent_at_ms: i64,
    message_deleted: bool,
    attachment: &synthetic::SyntheticAttachment,
    view: ChatListKind,
    month_dir: &ItemId,
    primary: bool,
    counter: &mut u64,
) {
    let key = common::attachment_key(chat.chat_id.0, message_id, attachment.index.0);
    let canonical_id = gramdrive_state::model::identity::ItemKey::Canonical(key).id();
    let item = appearance_id(view, key);
    let (view_kind, folder_id) = view_columns(view);

    // The provider item first: the engine rows below reference it.
    tx.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, view_kind, view_folder_id, display_name,
                            safe_name, is_directory, mime_type, logical_size,
                            metadata_version, content_version, availability, created_at_ms,
                            deleted_at_ms, tombstone_provenance)
         VALUES (?1, ?2, ?3, 'attachment', ?4, ?5, ?6, ?7, ?8, ?8, 0, ?9, ?10, 'm1', ?11,
                 ?12, ?13, ?14, ?15)",
        params![
            item.as_bytes(),
            ACCOUNT_ID,
            NAMESPACE,
            month_dir.as_bytes(),
            canonical_id.as_bytes(),
            view_kind,
            folder_id,
            attachment.original_name,
            attachment.mime_type,
            attachment.size as i64,
            attachment.content_version,
            if chat.is_protected {
                "restricted"
            } else {
                "fetchable"
            },
            sent_at_ms,
            message_deleted.then_some(sent_at_ms + 86_400_000),
            message_deleted.then_some("reconcile"),
        ],
    )
    .expect("attachment item");

    // Canonical facts and engine state only once, from the primary view.
    if primary {
        *counter += 1;
        let n = *counter;

        // Every 9th attachment has completed download: a blob row, the
        // attachment→blob link, and a cache entry (every 5th of those
        // pinned by a user).
        let blob_hash: Option<[u8; 32]> = n.is_multiple_of(9).then(|| {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&n.to_be_bytes());
            hash[8..16].copy_from_slice(&chat.chat_id.0.to_be_bytes());
            hash[16..24].copy_from_slice(&message_id.to_be_bytes());
            hash
        });
        if let Some(hash) = &blob_hash {
            tx.execute(
                "INSERT INTO blobs (account_id, hash_algo, hash, size, first_seen_at_ms)
                 VALUES (?1, 'sha256', ?2, ?3, ?4)",
                params![
                    ACCOUNT_ID,
                    hash.as_slice(),
                    attachment.size as i64,
                    sent_at_ms
                ],
            )
            .expect("blob");
        }

        tx.execute(
            "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                      attachment_index, original_name, mime_type,
                                      logical_size, content_version, telegram_unique_id,
                                      telegram_file_id, availability, can_be_saved,
                                      blob_hash_algo, blob_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                ACCOUNT_ID,
                NAMESPACE,
                chat.chat_id.0,
                message_id,
                attachment.index.0,
                attachment.original_name,
                attachment.mime_type,
                attachment.size as i64,
                attachment.content_version,
                format!("uniq-{n}"),
                format!("file-{n}"),
                if chat.is_protected {
                    "restricted"
                } else {
                    "fetchable"
                },
                (!chat.is_protected) as i64,
                blob_hash.as_ref().map(|_| "sha256"),
                blob_hash.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )
        .expect("attachment");

        if let Some(hash) = &blob_hash {
            let pinned = n.is_multiple_of(45);
            tx.execute(
                "INSERT INTO cache_entries (item_id, account_id, content_version, kind, size,
                                            blob_hash_algo, blob_hash, verification, pinned,
                                            pin_origin, last_access_at_ms, materialized_at_ms)
                 VALUES (?1, ?2, ?3, 'blob', ?4, 'sha256', ?5, 'verified', ?6, ?7, ?8, ?8)",
                params![
                    item.as_bytes(),
                    ACCOUNT_ID,
                    attachment.content_version,
                    attachment.size as i64,
                    hash.as_slice(),
                    pinned as i64,
                    pinned.then_some("user"),
                    sent_at_ms + i64::from(attachment.index.0),
                ],
            )
            .expect("cache entry");
            if pinned {
                tx.execute(
                    "INSERT INTO pins (item_id, origin, created_at_ms) VALUES (?1, 'user', ?2)",
                    params![item.as_bytes(), sent_at_ms],
                )
                .expect("pin");
            }
        } else if !chat.is_protected {
            // Undownloaded and unprotected: some are in flight.
            let transfer_state = match n % 13 {
                0 => Some(("queued", None)),
                1 => Some(("failed", Some("rate_limited"))),
                2 => Some(("running", None)),
                _ => None,
            };
            if let Some((state, failure)) = transfer_state {
                tx.execute(
                    "INSERT INTO transfers (item_id, content_version, state, priority,
                                            requested_ranges, retry_count, next_retry_at_ms,
                                            failure_category, created_at_ms, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        item.as_bytes(),
                        attachment.content_version,
                        state,
                        (n % 5) as i64,
                        format!("[[0,{}]]", attachment.size),
                        i64::from(state == "failed"),
                        (state == "failed").then_some(sent_at_ms + 60_000),
                        failure,
                        sent_at_ms,
                    ],
                )
                .expect("transfer");
            }
        }
    }
}
