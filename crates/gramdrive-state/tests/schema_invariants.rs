//! The schema enforces its own invariants (TASK-260715-1ceq7h AC): foreign
//! keys, uniqueness, CHECK constraints, the POL-3 append-only trigger, and
//! deletion cascades — all proven against the real database, not promised
//! by repository code.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the shared `common` helpers are neither: they
// sit at module level in an integration-test binary. The rationale still
// applies in full — this file links into no product artifact — so the
// exemption is restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{
    ACCOUNT_ID, NAMESPACE, account_root_id, appearance_id, canonical_chat_id, chat_canonical_key,
    expect_rejected, insert_chat, insert_message, insert_observed_event, store_with_account,
};
use gramdrive_state::StateStore;
use gramdrive_state::model::identity::{ChatListKind, FolderId};
use rusqlite::params;

// ---------------------------------------------------------------------------
// Foreign keys
// ---------------------------------------------------------------------------

#[test]
fn item_requires_its_account() {
    let store = StateStore::open_in_memory().expect("open");
    let result = store.connection().execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (?1, 99, 1, 'account', NULL, 'x', 'x', 1, 'm1')",
        params![account_root_id().as_bytes()],
    );
    expect_rejected(result, "FOREIGN KEY");
}

#[test]
fn item_parent_must_exist() {
    let store = store_with_account();
    let result = store.connection().execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (?1, ?2, ?3, 'chat', ?4, 'Chat', 'Chat', 1, 'm1')",
        params![
            canonical_chat_id(100).as_bytes(),
            ACCOUNT_ID,
            NAMESPACE,
            b"missing-parent".as_slice(),
        ],
    );
    expect_rejected(result, "FOREIGN KEY");
}

#[test]
fn message_requires_its_chat_and_event() {
    let store = store_with_account();
    let conn = store.connection();

    // No chat row yet: the chat FK fires.
    let result = conn.execute(
        "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                               sent_at_ms, latest_event_seq)
         VALUES (?1, ?2, 100, 5, 1000, 1)",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "FOREIGN KEY");

    // Chat present but the referenced event is missing: the event FK fires.
    insert_chat(conn, 100);
    let result = conn.execute(
        "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                               sent_at_ms, latest_event_seq)
         VALUES (?1, ?2, 100, 5, 1000, 424242)",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "FOREIGN KEY");
}

#[test]
fn attachment_blob_link_requires_the_blob_row() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);
    let seq = insert_observed_event(conn, 100, 5);
    insert_message(conn, 100, 5, seq);

    let hash = [0xabu8; 32];
    let result = conn.execute(
        "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                  attachment_index, content_version, blob_hash_algo, blob_hash)
         VALUES (?1, ?2, 100, 5, 0, 'c1', 'sha256', ?3)",
        params![ACCOUNT_ID, NAMESPACE, hash.as_slice()],
    );
    expect_rejected(result, "FOREIGN KEY");

    // With the blob row present the same insert is welcome.
    conn.execute(
        "INSERT INTO blobs (account_id, hash_algo, hash, size, first_seen_at_ms)
         VALUES (?1, 'sha256', ?2, 11, 1000)",
        params![ACCOUNT_ID, hash.as_slice()],
    )
    .expect("insert blob");
    conn.execute(
        "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                  attachment_index, content_version, blob_hash_algo, blob_hash)
         VALUES (?1, ?2, 100, 5, 0, 'c1', 'sha256', ?3)",
        params![ACCOUNT_ID, NAMESPACE, hash.as_slice()],
    )
    .expect("attachment with a real blob");
}

// ---------------------------------------------------------------------------
// CHECK constraints
// ---------------------------------------------------------------------------

#[test]
fn item_kind_vocabulary_is_closed() {
    let store = store_with_account();
    let result = store.connection().execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (x'01', ?1, ?2, 'message', ?3, 'x', 'x', 0, 'm1')",
        params![ACCOUNT_ID, NAMESPACE, account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn appearance_columns_come_as_a_unit() {
    let store = store_with_account();
    let conn = store.connection();

    // canonical_item_id without a view.
    let result = conn.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, display_name, safe_name, is_directory,
                            metadata_version)
         VALUES (x'02', ?1, ?2, 'chat', ?3, ?4, 'Chat', 'Chat', 1, 'm1')",
        params![
            ACCOUNT_ID,
            NAMESPACE,
            account_root_id().as_bytes(),
            canonical_chat_id(100).as_bytes(),
        ],
    );
    expect_rejected(result, "CHECK");

    // A folder view demands a folder id; a built-in view forbids one.
    let result = conn.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, view_kind, view_folder_id, display_name,
                            safe_name, is_directory, metadata_version)
         VALUES (x'03', ?1, ?2, 'chat', ?3, ?4, 'folder', NULL, 'Chat', 'Chat', 1, 'm1')",
        params![
            ACCOUNT_ID,
            NAMESPACE,
            account_root_id().as_bytes(),
            canonical_chat_id(100).as_bytes(),
        ],
    );
    expect_rejected(result, "CHECK");

    let result = conn.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            canonical_item_id, view_kind, view_folder_id, display_name,
                            safe_name, is_directory, metadata_version)
         VALUES (x'04', ?1, ?2, 'chat', ?3, ?4, 'main', 5, 'Chat', 'Chat', 1, 'm1')",
        params![
            ACCOUNT_ID,
            NAMESPACE,
            account_root_id().as_bytes(),
            canonical_chat_id(100).as_bytes(),
        ],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn directories_carry_no_content_facts() {
    let store = store_with_account();
    let result = store.connection().execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version,
                            content_version)
         VALUES (x'05', ?1, ?2, 'chat', ?3, 'Chat', 'Chat', 1, 'm1', 'c1')",
        params![ACCOUNT_ID, NAMESPACE, account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");

    // is_directory must agree with kind.
    let result = store.connection().execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (x'06', ?1, ?2, 'chat', ?3, 'Chat', 'Chat', 0, 'm1')",
        params![ACCOUNT_ID, NAMESPACE, account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn account_retention_mode_is_pol3_vocabulary() {
    let store = StateStore::open_in_memory().expect("open");
    let result = store.connection().execute(
        "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                               namespace_version, retention_mode, created_at_ms, updated_at_ms)
         VALUES (1, 'local_tdlib', 'A', 'authorized', 0, 'keep-everything', 0, 0)",
        [],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn transfer_state_and_failure_category_agree() {
    let store = store_with_account();
    let conn = store.connection();

    // failed without a category: unclassifiable for retry policy (SYNC-044).
    let result = conn.execute(
        "INSERT INTO transfers (item_id, content_version, state, requested_ranges,
                                created_at_ms, updated_at_ms)
         VALUES (?1, 'c1', 'failed', '[[0,1024]]', 0, 0)",
        params![account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");

    // done with a leftover category: a lie about the terminal state.
    let result = conn.execute(
        "INSERT INTO transfers (item_id, content_version, state, requested_ranges,
                                failure_category, created_at_ms, updated_at_ms)
         VALUES (?1, 'c1', 'done', '[[0,1024]]', 'network', 0, 0)",
        params![account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");

    // Ranges must at least be JSON.
    let result = conn.execute(
        "INSERT INTO transfers (item_id, content_version, state, requested_ranges,
                                created_at_ms, updated_at_ms)
         VALUES (?1, 'c1', 'queued', 'not json', 0, 0)",
        params![account_root_id().as_bytes()],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn cache_pin_flag_and_origin_agree() {
    let store = store_with_account();
    let conn = store.connection();

    let result = conn.execute(
        "INSERT INTO cache_entries (item_id, account_id, content_version, kind, size,
                                    pinned, last_access_at_ms, materialized_at_ms)
         VALUES (?1, ?2, 'c1', 'blob', 10, 1, 0, 0)",
        params![account_root_id().as_bytes(), ACCOUNT_ID],
    );
    expect_rejected(result, "CHECK");

    let result = conn.execute(
        "INSERT INTO cache_entries (item_id, account_id, content_version, kind, size,
                                    pinned, pin_origin, last_access_at_ms, materialized_at_ms)
         VALUES (?1, ?2, 'c1', 'blob', 10, 0, 'user', 0, 0)",
        params![account_root_id().as_bytes(), ACCOUNT_ID],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn blob_hash_length_matches_the_algorithm() {
    let store = store_with_account();
    let result = store.connection().execute(
        "INSERT INTO blobs (account_id, hash_algo, hash, size, first_seen_at_ms)
         VALUES (?1, 'sha256', ?2, 10, 0)",
        params![ACCOUNT_ID, [0u8; 31].as_slice()],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn sync_window_bounds_are_ordered_and_paired() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);

    let result = conn.execute(
        "INSERT INTO chat_sync_state (account_id, namespace_version, chat_id,
                                      oldest_loaded_message_id, newest_loaded_message_id)
         VALUES (?1, ?2, 100, 50, 10)",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "CHECK");

    let result = conn.execute(
        "INSERT INTO chat_sync_state (account_id, namespace_version, chat_id,
                                      oldest_loaded_message_id, newest_loaded_message_id)
         VALUES (?1, ?2, 100, 10, NULL)",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "CHECK");
}

// ---------------------------------------------------------------------------
// Uniqueness
// ---------------------------------------------------------------------------

#[test]
fn live_siblings_may_not_share_a_name() {
    let store = store_with_account();
    let conn = store.connection();
    let root = account_root_id();

    let insert = |item: &[u8], deleted: Option<i64>| {
        conn.execute(
            "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                                display_name, safe_name, is_directory, metadata_version,
                                deleted_at_ms)
             VALUES (?1, ?2, ?3, 'chat', ?4, 'Team', 'Team', 1, 'm1', ?5)",
            params![item, ACCOUNT_ID, NAMESPACE, root.as_bytes(), deleted],
        )
    };

    insert(canonical_chat_id(100).as_bytes(), None).expect("first sibling");
    expect_rejected(insert(canonical_chat_id(101).as_bytes(), None), "UNIQUE");
    // A tombstoned row does not block a live successor (POL-3 tombstones
    // keep their name; the partial index skips them).
    insert(canonical_chat_id(102).as_bytes(), Some(5000)).expect("tombstoned namesake");
}

#[test]
fn one_appearance_per_canonical_item_and_view() {
    let store = store_with_account();
    let conn = store.connection();
    let root = account_root_id();
    let canonical = chat_canonical_key(100);

    let insert = |item: &[u8], name: &str, view: &str, folder: Option<i32>| {
        conn.execute(
            "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                                canonical_item_id, view_kind, view_folder_id, display_name,
                                safe_name, is_directory, metadata_version)
             VALUES (?1, ?2, ?3, 'chat', ?4, ?5, ?6, ?7, 'Team', ?8, 1, 'm1')",
            params![
                item,
                ACCOUNT_ID,
                NAMESPACE,
                root.as_bytes(),
                canonical_chat_id(100).as_bytes(),
                view,
                folder,
                name,
            ],
        )
    };

    insert(
        appearance_id(ChatListKind::Main, canonical).as_bytes(),
        "Team",
        "main",
        None,
    )
    .expect("main appearance");

    // The same canonical chat again in Main — even under another name and
    // key — is a duplicate appearance. Without the COALESCE sentinel in the
    // unique index the NULL folder ids would make this pass.
    expect_rejected(
        insert(
            appearance_id(ChatListKind::Folder(FolderId(9)), canonical).as_bytes(),
            "Team (2)",
            "main",
            None,
        ),
        "UNIQUE",
    );

    // A different view of the same canonical chat is fine (DOM-022).
    insert(
        appearance_id(ChatListKind::Archive, canonical).as_bytes(),
        "Team (3)",
        "archive",
        None,
    )
    .expect("archive appearance");
}

#[test]
fn one_cursor_per_account_and_stream() {
    let store = store_with_account();
    let conn = store.connection();

    let upsert = |cursor: &str| {
        conn.execute(
            "INSERT INTO change_cursors (account_id, namespace_version, stream, cursor_text,
                                         updated_at_ms)
             VALUES (?1, ?2, 'drive', ?3, 0)
             ON CONFLICT (account_id, stream) DO UPDATE
                 SET cursor_text = excluded.cursor_text,
                     namespace_version = excluded.namespace_version,
                     updated_at_ms = excluded.updated_at_ms",
            params![ACCOUNT_ID, NAMESPACE, cursor],
        )
        .expect("upsert cursor")
    };

    upsert("gdc-first");
    upsert("gdc-second");
    let (count, text): (i64, String) = conn
        .query_row(
            "SELECT count(*), max(cursor_text) FROM change_cursors WHERE account_id = ?1",
            params![ACCOUNT_ID],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read cursor");
    assert_eq!(count, 1, "SYNC-022: one durable position per stream");
    assert_eq!(text, "gdc-second");
}

// ---------------------------------------------------------------------------
// POL-3: the append-only event log
// ---------------------------------------------------------------------------

#[test]
fn event_log_rejects_rewrites_but_allows_payload_purge() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);
    let seq = insert_observed_event(conn, 100, 5);

    // Rewriting history is refused wholesale.
    let result = conn.execute(
        "UPDATE message_events SET event_kind = 'edited' WHERE event_seq = ?1",
        params![seq],
    );
    expect_rejected(result, "append-only");

    let result = conn.execute(
        "UPDATE message_events SET payload = x'99' WHERE event_seq = ?1",
        params![seq],
    );
    expect_rejected(result, "append-only");

    let result = conn.execute(
        "UPDATE message_events SET observed_at_ms = 9999 WHERE event_seq = ?1",
        params![seq],
    );
    expect_rejected(result, "append-only");

    // The one sanctioned update: the Mirror-mode content purge (POL-3) —
    // payload and schema go to NULL together, the marker stays.
    conn.execute(
        "UPDATE message_events SET payload = NULL, payload_schema = NULL WHERE event_seq = ?1",
        params![seq],
    )
    .expect("payload purge");
    let (kind, payload): (String, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT event_kind, payload FROM message_events WHERE event_seq = ?1",
            params![seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read purged event");
    assert_eq!(kind, "observed", "the marker survives the purge");
    assert_eq!(payload, None, "the content does not");
}

#[test]
fn deletion_tombstones_never_carry_content() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);

    let result = conn.execute(
        "INSERT INTO message_events (account_id, namespace_version, chat_id, message_id,
                                     event_kind, observed_at_ms, payload_schema, payload)
         VALUES (?1, ?2, 100, 5, 'deleted', 1000, 1, x'01')",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "CHECK");

    // And payload always travels with its schema stamp.
    let result = conn.execute(
        "INSERT INTO message_events (account_id, namespace_version, chat_id, message_id,
                                     event_kind, observed_at_ms, payload_schema, payload)
         VALUES (?1, ?2, 100, 5, 'observed', 1000, NULL, x'01')",
        params![ACCOUNT_ID, NAMESPACE],
    );
    expect_rejected(result, "CHECK");
}

#[test]
fn event_sequence_numbers_are_never_reused() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);

    let first = insert_observed_event(conn, 100, 5);
    conn.execute(
        "DELETE FROM message_events WHERE event_seq = ?1",
        params![first],
    )
    .expect("purge the row entirely");
    let second = insert_observed_event(conn, 100, 6);
    assert!(
        second > first,
        "watermarks depend on sequence numbers never going backwards \
         (got {second} after deleting {first})"
    );
}

#[test]
fn current_state_pins_its_event_against_purge() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);
    let seq = insert_observed_event(conn, 100, 5);
    insert_message(conn, 100, 5, seq);

    // The event backing a message's current state cannot be deleted from
    // under it; purge must retarget or remove the projection first.
    let result = conn.execute(
        "DELETE FROM message_events WHERE event_seq = ?1",
        params![seq],
    );
    expect_rejected(result, "FOREIGN KEY");
}

// ---------------------------------------------------------------------------
// Cascades
// ---------------------------------------------------------------------------

#[test]
fn deleting_an_account_removes_every_scoped_row() {
    let store = store_with_account();
    let conn = store.connection();
    insert_chat(conn, 100);
    let seq = insert_observed_event(conn, 100, 5);
    insert_message(conn, 100, 5, seq);
    conn.execute(
        "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                  attachment_index, content_version)
         VALUES (?1, ?2, 100, 5, 0, 'c1')",
        params![ACCOUNT_ID, NAMESPACE],
    )
    .expect("insert attachment");
    conn.execute(
        "INSERT INTO change_cursors (account_id, namespace_version, stream, cursor_text,
                                     updated_at_ms)
         VALUES (?1, ?2, 'drive', 'gdc-x', 0)",
        params![ACCOUNT_ID, NAMESPACE],
    )
    .expect("insert cursor");

    conn.execute(
        "DELETE FROM accounts WHERE account_id = ?1",
        params![ACCOUNT_ID],
    )
    .expect("delete account");

    for table in [
        "items",
        "chats",
        "messages",
        "message_events",
        "attachments",
        "change_cursors",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0, "{table} must be empty after the account cascade");
    }
}

#[test]
fn deleting_a_parent_item_removes_its_subtree_and_attached_state() {
    let store = store_with_account();
    let conn = store.connection();
    let root = account_root_id();
    let chat_item = canonical_chat_id(100);

    conn.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (?1, ?2, ?3, 'chat', ?4, 'Team', 'Team', 1, 'm1')",
        params![chat_item.as_bytes(), ACCOUNT_ID, NAMESPACE, root.as_bytes()],
    )
    .expect("chat item");
    conn.execute(
        "INSERT INTO pins (item_id, origin, created_at_ms) VALUES (?1, 'user', 0)",
        params![chat_item.as_bytes()],
    )
    .expect("pin");
    conn.execute(
        "INSERT INTO transfers (item_id, content_version, state, requested_ranges,
                                created_at_ms, updated_at_ms)
         VALUES (?1, 'c1', 'queued', '[[0,10]]', 0, 0)",
        params![chat_item.as_bytes()],
    )
    .expect("transfer");

    conn.execute(
        "DELETE FROM items WHERE item_id = ?1",
        params![root.as_bytes()],
    )
    .expect("delete root");

    for table in ["items", "pins", "transfers"] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0, "{table} must follow the item cascade");
    }
}
