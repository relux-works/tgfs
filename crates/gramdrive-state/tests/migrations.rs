//! The migration framework through the public surface: what a host and
//! reconciliation actually touch (TASK-260715-18l9xz; SYNC-072, SYNC-071,
//! NFR-041).
//!
//! The runner's own mechanics — chunking, checkpoints, interruption, resume
//! — are unit-tested next to the code in `src/migrate.rs`, where the
//! internal registry is reachable. What is here is the contract everything
//! outside the crate depends on: a v1 file opens, a journal appears on a
//! file that predates the journal, a file from the future is refused without
//! being written to, and repair markers survive and read back.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the helpers below are neither: they sit at
// module level in an integration-test binary. The rationale still applies in
// full — this file links into no product artifact — so the exemption is
// restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_state::model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey,
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, DocFormat, DocPartition,
    GeneratedDocKey, ItemKey, MediaDirKey, MessageId, MessageKey, NamespaceVersion, SchemaFamily,
    YearDirKey,
};
use gramdrive_state::{RepairKind, SCHEMA_VERSION, StateError, StateStore};
use rusqlite::{Connection, params};

/// Representative rows of a v1 database — the fixture a v2 migration will be
/// written against. See `fixtures/v1_seed.sql`.
const V1_SEED_SQL: &str = include_str!("../fixtures/v1_seed.sql");
const V1_SCHEMA_SQL: &str = include_str!("../src/schema/v1.sql");
const JOURNAL_SQL: &str = include_str!("../src/schema/journal.sql");
const V2_SCHEMA_SQL: &str = include_str!("../src/schema/v2.sql");
const V3_SCHEMA_SQL: &str = include_str!("../src/schema/v3.sql");

/// A unique database path under the OS temp directory, cleaned by
/// [`TempDb::drop`]. Uniqueness comes from the process id and a counter —
/// no clock, no randomness, so parallel test binaries cannot collide.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-migrations-test-{}-{n}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    /// A v1 database with the fixture rows in it.
    fn seeded(&self) -> StateStore {
        let store = StateStore::open(&self.path).expect("create v1");
        store
            .connection()
            .execute_batch(V1_SEED_SQL)
            .expect("v1 seed rows");
        store
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_schema query")
        == 1
}

fn seed_legacy_v3(path: &std::path::Path) -> LegacyIds {
    let conn = Connection::open(path).expect("open v3 fixture");
    conn.execute_batch(V1_SCHEMA_SQL).expect("v1 schema");
    conn.execute_batch(JOURNAL_SQL).expect("journal");
    conn.execute_batch(V2_SCHEMA_SQL).expect("v2 schema");
    conn.execute_batch(V3_SCHEMA_SQL).expect("v3 schema");
    conn.execute_batch(
        "INSERT INTO schema_history (version, applied_at_ms) VALUES (2, 2), (3, 3);
         PRAGMA user_version = 3;",
    )
    .expect("stamp v3");

    let scope = AccountScope {
        account: AccountKey {
            account_id: AccountId(7),
        },
        namespace_version: NamespaceVersion(1),
    };
    let chat = ChatKey {
        scope,
        chat_id: ChatId(100),
    };
    let message = MessageKey {
        chat,
        message_id: MessageId(500),
    };
    let view = ChatListKind::Main;
    let account = ItemKey::Canonical(CanonicalKey::Account(scope.account)).id();
    let list = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey { scope, kind: view })).id();
    let chat_item = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::Chat(chat),
    })
    .id();
    let year_key = CanonicalKey::YearDir(YearDirKey { chat, year: 2026 });
    let year = ItemKey::Appearance(AppearanceKey {
        view,
        item: year_key,
    })
    .id();
    let media_key = CanonicalKey::MediaDir(MediaDirKey { chat, year: 2026 });
    let media = ItemKey::Appearance(AppearanceKey {
        view,
        item: media_key,
    })
    .id();
    let whole_key = CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat,
        partition: DocPartition::Chat,
        format: DocFormat::Ndjson,
        schema_family: SchemaFamily(1),
    });
    let whole_ndjson = ItemKey::Appearance(AppearanceKey {
        view,
        item: whole_key,
    })
    .id();
    let month_markdown_key = CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat,
        partition: DocPartition::Month {
            year: 2026,
            month: 7,
        },
        format: DocFormat::Markdown,
        schema_family: SchemaFamily(1),
    });
    let month_markdown = ItemKey::Appearance(AppearanceKey {
        view,
        item: month_markdown_key,
    })
    .id();
    let attachment_key = CanonicalKey::Attachment(AttachmentKey {
        message,
        index: AttachmentIndex(0),
    });
    let attachment = ItemKey::Appearance(AppearanceKey {
        view,
        item: attachment_key,
    })
    .id();

    conn.execute(
        "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                               namespace_version, created_at_ms, updated_at_ms)
         VALUES (7, 'local_tdlib', 'Authorized', 'authorized', 1, 1, 1)",
        [],
    )
    .expect("account");
    conn.execute(
        "INSERT INTO chats (account_id, namespace_version, chat_id, chat_type, title,
                            metadata_version)
         VALUES (7, 1, 100, 'private', 'Chat', 'chat-v3')",
        [],
    )
    .expect("chat");
    conn.execute(
        "INSERT INTO message_events (account_id, namespace_version, chat_id, message_id,
                                     event_kind, observed_at_ms, payload_schema, payload)
         VALUES (7, 1, 100, 500, 'observed', 1, 1, X'01')",
        [],
    )
    .expect("event");
    conn.execute(
        "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                               sent_at_ms, latest_event_seq)
         VALUES (7, 1, 100, 500, unixepoch('2026-07-21 09:08:07') * 1000, 1)",
        [],
    )
    .expect("message");
    conn.execute(
        "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
             attachment_index, original_name, mime_type, logical_size, content_version)
         VALUES (7, 1, 100, 500, 0, 'photo.jpg', 'image/jpeg', 1234, 'bytes-v3')",
        [],
    )
    .expect("attachment facts");

    let insert_item = |id: &[u8],
                       kind: &str,
                       parent: Option<&[u8]>,
                       canonical: Option<&[u8]>,
                       name: &str,
                       directory: bool,
                       mime: Option<&str>,
                       content: Option<&str>| {
        conn.execute(
            "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name, is_directory,
                 mime_type, metadata_version, content_version)
             VALUES (?1, 7, 1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, 'projection-v3', ?9)",
            params![
                id,
                kind,
                parent,
                canonical,
                canonical.map(|_| "main"),
                name,
                directory,
                mime,
                content,
            ],
        )
        .expect("item");
    };
    insert_item(
        account.as_bytes(),
        "account",
        None,
        None,
        "Authorized",
        true,
        None,
        None,
    );
    insert_item(
        list.as_bytes(),
        "chat_list",
        Some(account.as_bytes()),
        None,
        "Chats",
        true,
        None,
        None,
    );
    let chat_canonical = ItemKey::Canonical(CanonicalKey::Chat(chat)).id();
    insert_item(
        chat_item.as_bytes(),
        "chat",
        Some(list.as_bytes()),
        Some(chat_canonical.as_bytes()),
        "Chat",
        true,
        None,
        None,
    );
    let year_canonical = ItemKey::Canonical(year_key).id();
    insert_item(
        year.as_bytes(),
        "year_dir",
        Some(chat_item.as_bytes()),
        Some(year_canonical.as_bytes()),
        "2026",
        true,
        None,
        None,
    );
    let media_canonical = ItemKey::Canonical(media_key).id();
    insert_item(
        media.as_bytes(),
        "media_dir",
        Some(year.as_bytes()),
        Some(media_canonical.as_bytes()),
        "media",
        true,
        None,
        None,
    );
    let whole_canonical = ItemKey::Canonical(whole_key).id();
    insert_item(
        whole_ndjson.as_bytes(),
        "generated_doc",
        Some(chat_item.as_bytes()),
        Some(whole_canonical.as_bytes()),
        "messages.ndjson",
        false,
        Some("application/x-ndjson"),
        Some("whole-v3"),
    );
    let markdown_canonical = ItemKey::Canonical(month_markdown_key).id();
    insert_item(
        month_markdown.as_bytes(),
        "generated_doc",
        Some(year.as_bytes()),
        Some(markdown_canonical.as_bytes()),
        "07.md",
        false,
        Some("text/markdown"),
        Some("month-v3"),
    );
    let attachment_canonical = ItemKey::Canonical(attachment_key).id();
    insert_item(
        attachment.as_bytes(),
        "attachment",
        Some(media.as_bytes()),
        Some(attachment_canonical.as_bytes()),
        "photo.jpg",
        false,
        Some("image/jpeg"),
        Some("bytes-v3"),
    );

    LegacyIds {
        scope,
        chat,
        chat_item,
        year,
        media,
        whole_ndjson,
        month_markdown,
        attachment,
    }
}

struct LegacyIds {
    scope: AccountScope,
    chat: ChatKey,
    chat_item: gramdrive_state::model::identity::ItemId,
    year: gramdrive_state::model::identity::ItemId,
    media: gramdrive_state::model::identity::ItemId,
    whole_ndjson: gramdrive_state::model::identity::ItemId,
    month_markdown: gramdrive_state::model::identity::ItemId,
    attachment: gramdrive_state::model::identity::ItemId,
}

#[test]
fn schema_v3_legacy_projection_migrates_atomically_to_date_first() {
    use gramdrive_state::model::identity::{ActiveStoriesKey, MonthDirKey};

    let db = TempDb::new();
    let legacy = seed_legacy_v3(&db.path);
    let store = StateStore::open(&db.path).expect("migrate v3 to v4");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

    let identity_counts: (i64, i64, i64) = store
        .connection()
        .query_row(
            "SELECT (SELECT count(*) FROM accounts), (SELECT count(*) FROM chats),
                    (SELECT count(*) FROM messages)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("identity counts");
    assert_eq!(identity_counts, (1, 1, 1));
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT display_timezone FROM accounts WHERE account_id = 7",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("timezone"),
        "UTC"
    );
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT render_generation FROM accounts WHERE account_id = 7",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("render generation"),
        0
    );

    let active = ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::ActiveStories(ActiveStoriesKey { chat: legacy.chat }),
    })
    .id();
    let month = ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::MonthDir(MonthDirKey {
            chat: legacy.chat,
            year: 2026,
            month: 7,
        }),
    })
    .id();
    for (id, kind, name) in [
        (&legacy.chat_item, "chat", "Chat"),
        (&active, "active_stories", "Active Stories"),
        (&month, "month_dir", "2026-07"),
    ] {
        let row: (String, String, Option<i64>) = store
            .connection()
            .query_row(
                "SELECT kind, display_name, deleted_at_ms FROM items WHERE item_id = ?1",
                [id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("live item");
        assert_eq!(row, (kind.to_owned(), name.to_owned(), None));
    }
    for id in [&legacy.year, &legacy.media, &legacy.whole_ndjson] {
        let deleted: Option<i64> = store
            .connection()
            .query_row(
                "SELECT deleted_at_ms FROM items WHERE item_id = ?1",
                [id.as_bytes()],
                |row| row.get(0),
            )
            .expect("legacy tombstone");
        assert!(deleted.is_some());
    }

    let markdown: (Vec<u8>, String) = store
        .connection()
        .query_row(
            "SELECT parent_item_id, display_name FROM items WHERE item_id = ?1",
            [legacy.month_markdown.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("markdown");
    assert_eq!(
        markdown,
        (month.as_bytes().to_vec(), "Messages.md".to_owned())
    );
    let monthly_ndjson = ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat: legacy.chat,
            partition: DocPartition::Month {
                year: 2026,
                month: 7,
            },
            format: DocFormat::Ndjson,
            schema_family: SchemaFamily(1),
        }),
    })
    .id();
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT display_name FROM items WHERE item_id = ?1 AND deleted_at_ms IS NULL",
                [monthly_ndjson.as_bytes()],
                |row| row.get::<_, String>(0),
            )
            .expect("monthly NDJSON"),
        "Messages.ndjson"
    );
    let attachment: (Vec<u8>, String) = store
        .connection()
        .query_row(
            "SELECT parent_item_id, display_name FROM items WHERE item_id = ?1",
            [legacy.attachment.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("attachment item");
    assert_eq!(attachment.0, month.as_bytes());
    assert_eq!(attachment.1, "2026-07-21 09-08-07 photo.jpg");
    let facts: (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = store
        .connection()
        .query_row(
            "SELECT logical_kind, telegram_representation, fidelity, source_name,
                    mime_type, exact_size FROM attachments",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("attachment facts");
    assert_eq!(
        facts,
        (
            "unknown".to_owned(),
            "unknown_legacy".to_owned(),
            "unknown_legacy".to_owned(),
            Some("photo.jpg".to_owned()),
            Some("image/jpeg".to_owned()),
            Some(1234)
        )
    );
    assert_eq!(legacy.scope.account.account_id, AccountId(7));

    let live_legacy: i64 = store
        .connection()
        .query_row(
            "SELECT count(*) FROM items WHERE deleted_at_ms IS NULL
           AND kind IN ('year_dir', 'media_dir')",
            [],
            |row| row.get(0),
        )
        .expect("legacy live count");
    assert_eq!(live_legacy, 0);
    let migrated_content: (Option<i64>, Option<i64>, bool, String, bool) = store
        .connection()
        .query_row(
            "SELECT s.oldest_loaded_message_id, s.newest_loaded_message_id,
                    s.history_complete, p.phase, p.retryable
             FROM chat_sync_state s
             JOIN chat_content_progress p USING (account_id, namespace_version, chat_id)
             WHERE s.account_id = 7 AND s.namespace_version = 1 AND s.chat_id = 100",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("v5 content migration");
    assert_eq!(
        migrated_content,
        (None, None, false, "pending".to_owned(), false)
    );
    let violations: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .expect("foreign key check");
    assert_eq!(violations, 0);
}

#[test]
fn the_v1_fixture_opens_at_the_current_version_with_its_rows_intact() {
    let db = TempDb::new();
    let store = db.seeded();
    drop(store);

    // The path a user's existing file takes: nothing to migrate, so nothing
    // is migrated, and nothing is disturbed.
    let store = StateStore::open(&db.path).expect("reopen the fixture");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    assert!(
        table_exists(store.connection(), "retention_purge_queue"),
        "v11 crash-resumable retention journal must migrate with the file"
    );
    assert!(
        table_exists(store.connection(), "retained_attachment_versions"),
        "v12 Audit attachment-version owners must migrate with the file"
    );

    let messages: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(messages, 12);

    let history: Vec<i64> = store
        .connection()
        .prepare("SELECT version FROM schema_history ORDER BY version")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        history,
        (1..=SCHEMA_VERSION).collect::<Vec<_>>(),
        "opening a current file must not record a second application"
    );

    assert!(
        store.repair_markers().expect("markers").is_empty(),
        "a healthy file owes nobody a repair"
    );
    assert!(
        store
            .connection()
            .query_row("SELECT count(*) FROM migration_progress", [], |row| row
                .get::<_, i64>(0))
            .expect("progress count")
            == 0,
        "a current file has no migration in flight"
    );
}

#[test]
fn v22_migrates_through_cache_index_and_durable_readiness_without_rewriting_cache_rows() {
    let db = TempDb::new();
    let store = db.seeded();
    store
        .connection()
        .execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name, is_directory,
                 mime_type, logical_size, metadata_version, content_version, availability
             )
             SELECT X'F0', account_id, namespace_version, 'account', NULL, NULL, NULL,
                    'migration fixture', 'migration-fixture-root', 1,
                    NULL, NULL, 'migration-root-v1', NULL, 'fetchable'
             FROM accounts LIMIT 1;
             INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name, is_directory,
                 mime_type, logical_size, metadata_version, content_version, availability
             )
             SELECT X'F1', account_id, namespace_version, 'generated_doc', X'F0', X'F2',
                    'main', 'migration fixture.txt', 'migration-fixture.txt', 0,
                    'text/plain', 1, 'migration-file-v1', 'migration-content-v1', 'fetchable'
             FROM accounts LIMIT 1;
             INSERT INTO cache_entries (
                 item_id, account_id, content_version, kind, size, verification,
                 pinned, last_access_at_ms, materialized_at_ms, materialization_ref
             )
             SELECT X'F1', account_id, 'migration-content-v1', 'generated_doc', 1,
                    'verified', 0, 1, 1, '/privacy-safe-migration-fixture'
             FROM accounts LIMIT 1;",
        )
        .expect("seed one materialization owner");
    let cache_rows_before: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM cache_entries", [], |row| row.get(0))
        .expect("cache count before migration");
    drop(store);

    // Recreate the exact installed v22 boundary. The v23 index and v24
    // readiness table plus their history/version stamps are removed; all
    // representative rows stay put.
    let legacy = Connection::open(&db.path).expect("open v22 boundary");
    legacy
        .execute_batch(
            "DROP INDEX cache_entries_by_materialization_ref;
             DROP TABLE namespace_readiness;
             DELETE FROM schema_history WHERE version IN (23, 24);
             PRAGMA user_version = 22;",
        )
        .expect("downgrade fixture stamp to v22");
    drop(legacy);

    let migrated = StateStore::open(&db.path).expect("migrate v22 to current");
    assert_eq!(
        migrated.schema_version().expect("schema version"),
        SCHEMA_VERSION
    );
    let cache_rows_after: i64 = migrated
        .connection()
        .query_row("SELECT count(*) FROM cache_entries", [], |row| row.get(0))
        .expect("cache count after migration");
    assert_eq!(cache_rows_after, cache_rows_before);
    let readiness_rows: i64 = migrated
        .connection()
        .query_row("SELECT count(*) FROM namespace_readiness", [], |row| {
            row.get(0)
        })
        .expect("readiness table after migration");
    assert_eq!(readiness_rows, 0, "migration never invents readiness");

    let plan: Vec<String> = migrated
        .connection()
        .prepare(
            "EXPLAIN QUERY PLAN SELECT EXISTS (
                 SELECT 1 FROM cache_entries WHERE materialization_ref = ?1)",
        )
        .expect("prepare claim plan")
        .query_map(["/privacy-safe-absent"], |row| row.get(3))
        .expect("query claim plan")
        .collect::<Result<_, _>>()
        .expect("collect claim plan");
    assert!(
        plan.iter().any(|step| {
            step.contains(
                "SEARCH cache_entries USING COVERING INDEX \
                           cache_entries_by_materialization_ref",
            )
        }),
        "v23 must turn the lease-critical ownership check into an indexed point probe: {plan:?}"
    );
    assert!(
        plan.iter()
            .all(|step| !(step == "SCAN cache_entries" || step.contains("USE TEMP B-TREE"))),
        "v23 must not retain the installed full scan or introduce a temp sort: {plan:?}"
    );
}

#[test]
fn a_file_written_before_the_journal_existed_gets_one() {
    let db = TempDb::new();
    let store = db.seeded();

    // Exactly what a database created by the build before this task looks
    // like: a valid v1 schema with no runner bookkeeping in it. The journal
    // cannot be introduced by a migration, because the runner needs the
    // journal to run one — so an open has to be able to add it.
    store
        .connection()
        .execute_batch("DROP TABLE migration_progress; DROP TABLE repair_markers;")
        .expect("drop the journal");
    drop(store);

    let store = StateStore::open(&db.path).expect("reopen a journal-less v1 file");

    assert!(table_exists(store.connection(), "migration_progress"));
    assert!(table_exists(store.connection(), "repair_markers"));
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    let messages: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(messages, 12, "adding the journal must not touch user data");
}

#[test]
fn a_file_from_a_newer_build_is_refused_before_anything_is_written_to_it() {
    let db = TempDb::new();
    let store = db.seeded();
    store
        .connection()
        .execute_batch("DROP TABLE migration_progress; DROP TABLE repair_markers;")
        .expect("drop the journal");
    store
        .connection()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 3)
        .expect("bump version");
    drop(store);

    match StateStore::open(&db.path) {
        Err(StateError::UnsupportedSchemaVersion { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 3);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }

    // The point of the check is that it happens *first*. If the refusal came
    // after the journal bootstrap, this build would have written to a file
    // whose schema it admits it does not understand.
    let conn = Connection::open(&db.path).expect("inspect");
    assert!(
        !table_exists(&conn, "migration_progress"),
        "a refused file must not be written to at all"
    );
    assert!(!table_exists(&conn, "repair_markers"));
}

#[test]
fn repair_markers_round_trip_and_survive_reopen() {
    let db = TempDb::new();
    let store = db.seeded();

    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("raise");
    let raised_at = {
        let markers = store.repair_markers().expect("markers");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].kind, RepairKind::RebuildProjection);
        assert_eq!(markers[0].detail, "items/account:7");
        markers[0].raised_at_ms
    };

    // Raising the same marker again is the same marker: reconciliation that
    // notices the same damage on every pass must not grow the list.
    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("raise again");
    let markers = store.repair_markers().expect("markers");
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0].raised_at_ms, raised_at,
        "a re-raise keeps the moment the problem started"
    );

    // A different detail is a different marker.
    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:9")
        .expect("raise other");
    assert_eq!(store.repair_markers().expect("markers").len(), 2);
    drop(store);

    let store = StateStore::open(&db.path).expect("reopen");
    assert_eq!(
        store.repair_markers().expect("markers").len(),
        2,
        "markers are durable: the repair is still owed after a restart"
    );

    store
        .clear_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("clear");
    let markers = store.repair_markers().expect("markers");
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].detail, "items/account:9");

    // Clearing what is not raised is not a failure.
    store
        .clear_repair_marker(RepairKind::MigrationInterrupted, "nothing like this")
        .expect("clear absent");
    assert_eq!(store.repair_markers().expect("markers").len(), 1);
}

#[test]
fn a_repair_marker_from_a_newer_build_is_reported_not_ignored() {
    let db = TempDb::new();
    let store = db.seeded();

    // A kind this build has never heard of, as a newer one would leave it.
    store
        .connection()
        .execute(
            "INSERT INTO repair_markers (kind, detail, raised_at_ms)
             VALUES ('rebuild_the_flux_capacitor', 'chat:100', 1704067200000)",
            [],
        )
        .expect("insert unknown kind");

    match store.repair_markers() {
        Err(StateError::UnknownRepairKind { kind }) => {
            assert_eq!(kind, "rebuild_the_flux_capacitor");
        }
        other => panic!("expected UnknownRepairKind, got {other:?}"),
    }
}

#[test]
fn error_messages_name_the_condition() {
    let err = StateError::MigrationFailed {
        version: 4,
        name: "backfill_render_hints",
        source: Box::new(StateError::MigrationStalled {
            checkpoint: "seq:9000".to_owned(),
        }),
    };
    assert_eq!(
        err.to_string(),
        "migration to version 4 (backfill_render_hints) failed: resumable migration reported \
         progress without moving its checkpoint ('seq:9000')"
    );

    let err = StateError::UnknownRepairKind {
        kind: "whatever".to_owned(),
    };
    assert_eq!(
        err.to_string(),
        "unknown repair marker kind 'whatever' in the database"
    );
}
