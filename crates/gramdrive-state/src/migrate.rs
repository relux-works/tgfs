//! The forward-only migration runner (TASK-260715-18l9xz; SYNC-072,
//! NFR-013, NFR-041).
//!
//! [`crate::schema`] creates a fresh database from the frozen baseline
//! script. Every version after that is a [`Migration`] in [`MIGRATIONS`],
//! applied in order. Forward-only, and that is a product decision, not a
//! missing feature: a downgrade would have to guess what a newer schema's
//! data means in an older shape, and the honest answer for a cache of
//! re-derivable state is to restore a backup or re-sync. An older build
//! meeting a newer file refuses it ([`StateError::UnsupportedSchemaVersion`])
//! rather than improvising (NFR-013).
//!
//! # Why a crash cannot corrupt a file
//!
//! `PRAGMA user_version` is part of the database header, so it commits with
//! the transaction that sets it. The runner never advances it except in the
//! same transaction as the work that earns it. That single rule is what
//! makes every interruption survivable:
//!
//! * [`MigrationStep::Sql`] is one transaction. A crash rolls it back
//!   whole; the next open sees the old version and starts it over.
//! * [`MigrationStep::Resumable`] cannot fit in one transaction — rewriting
//!   a column across 100k rows would hold a write lock for the duration and
//!   lose everything to one crash at the end. So it commits in chunks, and
//!   each chunk commits its data changes *together with* the checkpoint it
//!   resumes from. A crash leaves the old version and the last committed
//!   checkpoint; the next open hands that checkpoint back to the same chunk
//!   function and it continues.
//!
//! Idempotent resume is therefore a contract between two halves. The runner
//! guarantees a chunk is only ever re-called with a checkpoint it actually
//! committed (never a later one, never a partial one). The chunk function
//! guarantees the work after a given checkpoint is repeatable — which for
//! the usual "process rows after this key" shape it already is.
//!
//! # Writing a migration
//!
//! Add a [`Migration`] to [`MIGRATIONS`] and bump [`crate::SCHEMA_VERSION`]:
//! a const assertion below rejects the build if you do one without the
//! other. Then add `fixtures/v{previous}_seed.sql` — a unit test in this
//! module fails until every migration has a fixture database of the schema
//! it migrates *from*, because a migration tested only against a database
//! this build created has never met the schema it exists for.

use std::collections::{BTreeMap, BTreeSet};

use gramdrive_model::identity::{
    ActiveStoriesKey, AppearanceKey, CanonicalKey, ChatListKind, DocFormat, DocPartition, ItemId,
    ItemKey, MonthDirKey, SchemaFamily,
};
use gramdrive_model::naming::{NameKind, SiblingName, resolve_siblings};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::StateError;
use crate::repair::{self, RepairKind};
use crate::schema::SCHEMA_VERSION;

/// The version the frozen baseline script (`schema/v1.sql`) creates.
///
/// Every migration in [`MIGRATIONS`] runs *after* this: version 1 is not
/// migrated to, it is created.
pub const BASELINE_VERSION: i64 = 1;

/// The runner's bookkeeping tables — see `schema/journal.sql` for why they
/// are not part of the numbered schema.
const JOURNAL_SQL: &str = include_str!("schema/journal.sql");

/// Every migration this build carries, in application order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    // v2 — the provider-visible item change journal (TASK-260715-rhcnhc):
    // pure DDL plus one seed row, so it fits one transaction. No backfill,
    // deliberately: items that predate the journal have no changes to
    // report — a provider without an anchor performs a full enumeration
    // anyway and takes the journal's current sequence as its first anchor.
    Migration {
        version: 2,
        name: "item_change_journal",
        step: MigrationStep::Sql(include_str!("schema/v2.sql")),
    },
    // v3 — durable Telegram folder catalog plus the metadata-bootstrap
    // checkpoint. Both are source metadata; neither table stores messages,
    // media, credentials, or account presentation data.
    Migration {
        version: 3,
        name: "telegram_namespace_metadata",
        step: MigrationStep::Sql(include_str!("schema/v3.sql")),
    },
    Migration {
        version: 4,
        name: "date_first_live_content_contract",
        step: MigrationStep::AtomicRebuild(migrate_date_first_contract),
    },
    Migration {
        version: 5,
        name: "chat_content_progress",
        step: MigrationStep::Sql(include_str!("schema/v5.sql")),
    },
    Migration {
        version: 6,
        name: "account_render_generation",
        step: MigrationStep::Sql(include_str!("schema/v6.sql")),
    },
    Migration {
        version: 7,
        name: "tdlib_local_attachment_locator",
        step: MigrationStep::Sql(include_str!("schema/v7.sql")),
    },
    Migration {
        version: 8,
        name: "canonical_story_ingestion",
        step: MigrationStep::Sql(include_str!("schema/v8.sql")),
    },
    Migration {
        version: 9,
        name: "story_locators_pins_and_list_progress",
        step: MigrationStep::Sql(include_str!("schema/v9.sql")),
    },
    // v10 — story appearance identity includes its Active/month location.
    // The generic appearance uniqueness rule remains valid for every other
    // provider node, but cannot cover the transition's retained active
    // tombstone and live monthly appearance at the same time.
    Migration {
        version: 10,
        name: "location_scoped_story_appearances",
        step: MigrationStep::Sql(include_str!("schema/v10.sql")),
    },
    // v11 — filesystem objects removed by destructive retention transitions
    // are journalled before their cache rows disappear. The coordinator can
    // therefore resume deletion after a crash without retaining a live cache
    // claim or guessing which account owned the bytes.
    Migration {
        version: 11,
        name: "retention_purge_queue",
        step: MigrationStep::Sql(include_str!("schema/v11.sql")),
    },
    // v12 — Audit retains superseded allowed attachment metadata and only
    // bytes that were already verified. Download locators are intentionally
    // absent, so the historical owner can never trigger eager hydration.
    Migration {
        version: 12,
        name: "retained_attachment_versions",
        step: MigrationStep::Sql(include_str!("schema/v12.sql")),
    },
    // v13 — only chats with a live Telegram list appearance enter history
    // scheduling. Existing cursors survive list removal/reappearance; the
    // scheduler reads current eligibility rather than deleting progress.
    Migration {
        version: 13,
        name: "listed_chat_history_eligibility",
        step: MigrationStep::Sql(include_str!("schema/v13.sql")),
    },
    // v14 — generated publication is a durable least-advanced-first queue.
    // Recently refreshed low-sorting chats no longer monopolize the worklist
    // while never-published months from ordinary chats wait behind them.
    Migration {
        version: 14,
        name: "fair_generated_render_worklist",
        step: MigrationStep::Sql(include_str!("schema/v14.sql")),
    },
    // v15 — the existing chat-metadata appearance keeps its stable item id
    // while becoming a dotfile. The migration also refreshes the coalesced
    // provider journal so an installed domain learns the rename without an
    // account reset or a duplicate visible item.
    Migration {
        version: 15,
        name: "hidden_chat_metadata_document",
        step: MigrationStep::Sql(include_str!("schema/v15.sql")),
    },
    // v16 — a directory carries the exact logical size of its indexed
    // descendants, so Finder can answer "how big is this chat?" from
    // metadata alone. The backfill is exact and identifier-preserving;
    // correspondence-derived directory timestamps stay with the projection,
    // which owns the account's civil-month partitioning.
    Migration {
        version: 16,
        name: "directory_aggregate_logical_size",
        step: MigrationStep::Sql(include_str!("schema/v16.sql")),
    },
    // v17 — the backward-crawl rotation is keyed on the turns it actually
    // handed out, not on the last time anything happened to a chat. Live
    // traffic used to reset the scheduling key, so the busiest chats — the
    // ones a user reads — were the ones that never got history.
    Migration {
        version: 17,
        name: "backfill_turn_scheduling_key",
        step: MigrationStep::Sql(include_str!("schema/v17.sql")),
    },
    // v18 — File Provider observability is aggregate-only. Tombstones retain
    // the policy pass that caused them, and chat-list replacement commits
    // retain before/after membership counts plus their completeness claim.
    Migration {
        version: 18,
        name: "provider_observability_provenance",
        step: MigrationStep::Sql(include_str!("schema/v18.sql")),
    },
    // v19 — a policy-refused generated document is not silently dropped from
    // render accounting. The durable skip marker removes it from the bounded
    // worklist while preserving its exclusion reason and time for progress and
    // diagnostics; an explicit requeue or successful publication clears it.
    Migration {
        version: 19,
        name: "render_policy_skip_bookkeeping",
        step: MigrationStep::Sql(include_str!("schema/v19.sql")),
    },
    // v20 — .chat.json render planning must probe only the direct children
    // of a chat appearance. The partial index excludes every unrelated item
    // and dead document from this hot path on installed large accounts.
    Migration {
        version: 20,
        name: "chat_render_catalog_children",
        step: MigrationStep::Sql(include_str!("schema/v20.sql")),
    },
    // v21 — Stories is a truthful, separate provider view driven by
    // storyListMain. Its rows must not be mistaken for ordinary Telegram
    // chat-list membership or seed message-history work.
    Migration {
        version: 21,
        name: "story_list_provider_view",
        step: MigrationStep::AtomicRebuild(migrate_story_list_provider_view),
    },
    // v22 — authorization finalization spans SQLite, the OS keychain, and
    // TDLib's directory tree. This journal supplies the durable decision
    // record that makes the cross-resource swap recoverable without ever
    // storing secret key bytes in ordinary state.
    Migration {
        version: 22,
        name: "auth_finalization_journal",
        step: MigrationStep::Sql(include_str!("schema/v22.sql")),
    },
    // v23 — generated publication reclamation checks every obsolete immutable
    // path against current cache ownership while holding the hand-off lease
    // mutex. The path lookup must therefore be an indexed point probe: a full
    // cache table scan both burns the namespace worker and prevents foreground
    // File Provider reads from acquiring their generated-file lease.
    Migration {
        version: 23,
        name: "generated_materialization_reference_lookup",
        step: MigrationStep::Sql(include_str!("schema/v23.sql")),
    },
];

/// [`SCHEMA_VERSION`] and [`MIGRATIONS`] are one fact stated twice, so the
/// build refuses to link them out of agreement. A migration added without a
/// version bump would never run; a version bump without a migration would
/// leave every existing file rejected as needing a migration that does not
/// exist. Both are caught here rather than by a user's database.
const _: () = {
    assert!(
        MIGRATIONS.len() == (SCHEMA_VERSION - BASELINE_VERSION) as usize,
        "SCHEMA_VERSION must equal BASELINE_VERSION + MIGRATIONS.len(): \
         adding a migration means bumping the version, and vice versa"
    );
    let mut index = 0;
    while index < MIGRATIONS.len() {
        assert!(
            MIGRATIONS[index].version == BASELINE_VERSION + 1 + index as i64,
            "MIGRATIONS must be contiguous and ascending from BASELINE_VERSION + 1"
        );
        index += 1;
    }
};

/// One forward step: a database at `version - 1` becomes a database at
/// `version`.
#[derive(Debug)]
pub struct Migration {
    /// The version this step produces. The runner applies it only to a
    /// database at `version - 1`.
    pub version: i64,
    /// A stable name for diagnostics and the journal. Never parsed — it
    /// exists so a failure names the step in a report a user can send.
    pub name: &'static str,
    /// How the step runs.
    pub step: MigrationStep,
}

/// How a [`Migration`] does its work.
///
/// The choice is about transaction size, not about how complicated the
/// migration is: everything that fits in one transaction should be
/// [`MigrationStep::Sql`], because rollback is simpler than resume.
#[derive(Debug)]
pub enum MigrationStep {
    /// One SQL script, one transaction. DDL, and data work small and bounded
    /// enough to hold a write lock for. All-or-nothing: an interruption
    /// rolls it back and the next open starts over.
    Sql(&'static str),
    /// One atomic migration that must rebuild tables referenced by foreign
    /// keys. The runner temporarily disables FK enforcement outside the
    /// transaction, then runs `foreign_key_check` before accepting the new
    /// version.
    AtomicRebuild(AtomicFn),
    /// Chunked work with a durable checkpoint between chunks, for data too
    /// large for one transaction (SYNC-072).
    Resumable {
        /// DDL the chunks need before they can run — typically the
        /// `ALTER TABLE` whose new column the chunks fill.
        ///
        /// Runs only when there is no checkpoint yet, in the same
        /// transaction as the first chunk's commit. So it is never applied
        /// twice: either that transaction commits, and every later resume
        /// finds a checkpoint and skips this, or it rolls back and the
        /// resumed run applies it again from a clean slate.
        prepare: Option<&'static str>,
        /// The work itself, called in a loop until it reports
        /// [`ChunkOutcome::Done`].
        chunk: ChunkFn,
    },
}

/// One chunk of a [`MigrationStep::Resumable`].
///
/// Receives the transaction its writes must go through, and the checkpoint
/// the last committed chunk returned — `None` on the first chunk of a fresh
/// run. Everything the chunk writes commits atomically with the checkpoint
/// it returns, and after a crash it is called again with exactly the last
/// checkpoint that committed, so the work following any checkpoint it ever
/// produces must be repeatable.
///
/// A chunk must be sized to finish well inside the busy timeout other
/// processes are waiting out; it holds a write lock for its duration.
///
/// It must also eventually return [`ChunkOutcome::Done`], and that is the
/// migration's obligation, not something the runner can check for it. The
/// runner catches the one non-termination it can recognize locally — a chunk
/// handing back the checkpoint it was given ([`StateError::MigrationStalled`])
/// — but a chunk that returns a *fresh* checkpoint forever is
/// indistinguishable from a long migration making progress, and the runner
/// will keep calling it. Any bound the runner could impose would be a guess
/// at what "too many chunks" means for a migration it has never seen. The
/// fixture and interruption tests every migration ships are where that bug
/// is supposed to die.
pub type ChunkFn = fn(&Transaction<'_>, Option<&str>) -> Result<ChunkOutcome, StateError>;

/// One bounded, all-or-nothing migration implemented in Rust.
type AtomicFn = fn(&Transaction<'_>) -> Result<(), StateError>;

/// What a chunk did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// Work remains, and the next chunk resumes from `checkpoint`.
    More {
        /// Opaque to the runner — only the migration that wrote it reads it.
        /// It must differ from the checkpoint the chunk was given: an
        /// unchanged checkpoint is a chunk that reported progress it did not
        /// make, and the runner stops with [`StateError::MigrationStalled`]
        /// rather than spin on it forever.
        checkpoint: String,
    },
    /// Nothing remains. The runner stamps the new version and clears the
    /// checkpoint in one transaction.
    Done,
}

/// Creates the runner's journal on a database that does not have one.
///
/// Idempotent and version-independent by design: a file written by a build
/// older than the runner has no journal, and the runner needs one to migrate
/// it. Call only once the version is known to be one this build may write to
/// — this is the first write to the file.
pub(crate) fn ensure_journal(conn: &Connection) -> Result<(), StateError> {
    conn.execute_batch(JOURNAL_SQL)?;
    Ok(())
}

/// Applies `migrations` until the database reaches `target`.
///
/// A database already at `target` is not touched. A database below it with
/// no migration for the next step is refused with
/// [`StateError::MigrationRequired`] rather than left silently behind — with
/// the const assertion above holding that is unreachable in a shipped build,
/// which is exactly why it must stay a typed error and not an assumption.
///
/// Assumes the version has already been checked against `target`
/// ([`crate::schema::ensure_schema`] does that) and that the journal exists.
pub(crate) fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    target: i64,
) -> Result<(), StateError> {
    let mut current = current_version(conn)?;
    while current < target {
        let next = current + 1;
        let migration = migrations
            .iter()
            .find(|candidate| candidate.version == next)
            .ok_or(StateError::MigrationRequired {
                found: current,
                supported: target,
            })?;
        apply(conn, migration).map_err(|source| StateError::MigrationFailed {
            version: migration.version,
            name: migration.name,
            source: Box::new(source),
        })?;
        current = migration.version;
    }
    Ok(())
}

fn apply(conn: &mut Connection, migration: &Migration) -> Result<(), StateError> {
    match migration.step {
        MigrationStep::Sql(sql) => {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            finish(&tx, migration)?;
            tx.commit()?;
            Ok(())
        }
        MigrationStep::AtomicRebuild(step) => apply_atomic_rebuild(conn, migration, step),
        MigrationStep::Resumable { prepare, chunk } => {
            apply_resumable(conn, migration, prepare, chunk)
        }
    }
}

fn apply_atomic_rebuild(
    conn: &mut Connection,
    migration: &Migration,
    step: AtomicFn,
) -> Result<(), StateError> {
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| {
        let tx = conn.transaction()?;
        step(&tx)?;
        let violation: Option<String> = tx
            .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
            .optional()?;
        if let Some(table) = violation {
            return Err(StateError::CorruptRow {
                table: "foreign_key_check",
                detail: format!(
                    "migration {} rebuild left a violation in {table}",
                    migration.version
                ),
            });
        }
        finish(&tx, migration)?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", true)?;
    result
}

#[derive(Debug)]
struct DateFirstUpdate {
    id: ItemId,
    parent: ItemId,
    display_name: String,
    safe_name: String,
}

fn migrate_date_first_contract(tx: &Transaction<'_>) -> Result<(), StateError> {
    tx.execute_batch(include_str!("schema/v4.sql"))?;

    let rows: Vec<(Vec<u8>, String)> = {
        let mut statement = tx.prepare(
            "SELECT item_id, display_name FROM items WHERE deleted_at_ms IS NULL ORDER BY item_id",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };

    let mut directories: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut updates = Vec::new();
    let mut retire = Vec::new();
    let mut chat_views = Vec::new();

    for (bytes, old_name) in rows {
        let id = ItemId::parse_bytes(&bytes).map_err(|error| StateError::CorruptRow {
            table: "items",
            detail: format!("item_id does not parse during v4 migration: {error}"),
        })?;
        match id.key() {
            ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::Chat(chat),
            }) => {
                chat_views.push((view, chat));
                insert_date_first_directory(
                    tx,
                    ItemKey::Appearance(AppearanceKey {
                        view,
                        item: CanonicalKey::ActiveStories(ActiveStoriesKey { chat }),
                    })
                    .id(),
                    id.clone(),
                    "Active Stories",
                )?;
            }
            ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::GeneratedDoc(doc),
            }) => match (doc.partition, doc.format) {
                (DocPartition::Month { year, month }, DocFormat::Markdown | DocFormat::Ndjson) => {
                    let parent = month_directory(view, doc.chat, year, month);
                    if directories.insert(parent.as_bytes().to_vec()) {
                        insert_date_first_directory(
                            tx,
                            parent.clone(),
                            ItemKey::Appearance(AppearanceKey {
                                view,
                                item: CanonicalKey::Chat(doc.chat),
                            })
                            .id(),
                            &format!("{year:04}-{month:02}"),
                        )?;
                    }
                    let name = if doc.format == DocFormat::Markdown {
                        "Messages.md"
                    } else {
                        "Messages.ndjson"
                    };
                    updates.push(DateFirstUpdate {
                        id,
                        parent,
                        display_name: name.to_owned(),
                        safe_name: name.to_owned(),
                    });
                }
                (DocPartition::Chat, DocFormat::Ndjson) => retire.push(id),
                _ => {}
            },
            ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::Attachment(attachment),
            }) => {
                let (stamp, year, month): (String, i64, i64) = tx.query_row(
                    "SELECT strftime('%Y-%m-%d %H-%M-%S', sent_at_ms / 1000, 'unixepoch'),
                            CAST(strftime('%Y', sent_at_ms / 1000, 'unixepoch') AS INTEGER),
                            CAST(strftime('%m', sent_at_ms / 1000, 'unixepoch') AS INTEGER)
                     FROM messages
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                       AND message_id = ?4",
                    params![
                        attachment.message.chat.scope.account.account_id.0,
                        i64::from(attachment.message.chat.scope.namespace_version.0),
                        attachment.message.chat.chat_id.0,
                        attachment.message.message_id.0,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                let year = u16::try_from(year).map_err(|_| StateError::CorruptRow {
                    table: "messages",
                    detail: "attachment year does not fit u16".to_owned(),
                })?;
                let month = u8::try_from(month).map_err(|_| StateError::CorruptRow {
                    table: "messages",
                    detail: "attachment month does not fit u8".to_owned(),
                })?;
                let parent = month_directory(view, attachment.message.chat, year, month);
                if directories.insert(parent.as_bytes().to_vec()) {
                    insert_date_first_directory(
                        tx,
                        parent.clone(),
                        ItemKey::Appearance(AppearanceKey {
                            view,
                            item: CanonicalKey::Chat(attachment.message.chat),
                        })
                        .id(),
                        &format!("{year:04}-{month:02}"),
                    )?;
                }
                updates.push(DateFirstUpdate {
                    id,
                    parent,
                    display_name: format!("{stamp} {old_name}"),
                    safe_name: String::new(),
                });
            }
            ItemKey::Appearance(AppearanceKey {
                item: CanonicalKey::YearDir(_) | CanonicalKey::MediaDir(_),
                ..
            }) => retire.push(id),
            _ => {}
        }
    }

    // The legacy layout had one giant chat NDJSON and only monthly Markdown.
    // Rebuild both bounded documents for every observed message month before
    // retiring that giant file, so authorized history never passes through a
    // mixed or incomplete live layout.
    for (view, chat) in chat_views {
        let months: Vec<(i64, i64)> = {
            let mut statement = tx.prepare(
                "SELECT DISTINCT
                        CAST(strftime('%Y', sent_at_ms / 1000, 'unixepoch') AS INTEGER),
                        CAST(strftime('%m', sent_at_ms / 1000, 'unixepoch') AS INTEGER)
                 FROM messages
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                 ORDER BY 1, 2",
            )?;
            statement
                .query_map(
                    params![
                        chat.scope.account.account_id.0,
                        i64::from(chat.scope.namespace_version.0),
                        chat.chat_id.0,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<Result<_, _>>()?
        };
        for (year, month) in months {
            let year = u16::try_from(year).map_err(|_| StateError::CorruptRow {
                table: "messages",
                detail: "message year does not fit u16".to_owned(),
            })?;
            let month = u8::try_from(month).map_err(|_| StateError::CorruptRow {
                table: "messages",
                detail: "message month does not fit u8".to_owned(),
            })?;
            let parent = month_directory(view, chat, year, month);
            if directories.insert(parent.as_bytes().to_vec()) {
                insert_date_first_directory(
                    tx,
                    parent.clone(),
                    ItemKey::Appearance(AppearanceKey {
                        view,
                        item: CanonicalKey::Chat(chat),
                    })
                    .id(),
                    &format!("{year:04}-{month:02}"),
                )?;
            }
            insert_date_first_document(
                tx,
                view,
                chat,
                parent.clone(),
                year,
                month,
                DocFormat::Markdown,
            )?;
            insert_date_first_document(tx, view, chat, parent, year, month, DocFormat::Ndjson)?;
        }
    }

    let mut groups: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
    for (index, update) in updates.iter().enumerate() {
        if update.safe_name.is_empty() {
            groups
                .entry(update.parent.as_bytes().to_vec())
                .or_default()
                .push(index);
        }
    }
    for indices in groups.values() {
        let siblings: Vec<_> = indices
            .iter()
            .map(|index| SiblingName {
                id: &updates[*index].id,
                raw: &updates[*index].display_name,
                kind: NameKind::File,
                fixed: false,
            })
            .collect();
        let names = resolve_siblings(&siblings);
        for (index, name) in indices.iter().zip(names) {
            updates[*index].safe_name = name.as_str().to_owned();
        }
    }

    for update in updates {
        tx.execute(
            "UPDATE items
             SET parent_item_id = ?2, display_name = ?3, safe_name = ?4,
                 metadata_version = 'date-first-v1'
             WHERE item_id = ?1",
            params![
                update.id.as_bytes(),
                update.parent.as_bytes(),
                update.display_name,
                update.safe_name,
            ],
        )?;
    }
    for id in retire {
        tx.execute(
            "UPDATE items
             SET deleted_at_ms = COALESCE(deleted_at_ms, unixepoch() * 1000),
                 metadata_version = 'date-first-v1'
             WHERE item_id = ?1",
            [id.as_bytes()],
        )?;
    }

    tx.execute("DELETE FROM item_changes", [])?;
    tx.execute(
        "INSERT INTO item_changes (item_id, account_id)
         SELECT item_id, account_id FROM items ORDER BY item_id",
        [],
    )?;
    Ok(())
}

fn migrate_story_list_provider_view(tx: &Transaction<'_>) -> Result<(), StateError> {
    tx.execute_batch(include_str!("schema/v21.sql"))?;
    Ok(())
}

fn month_directory(
    view: ChatListKind,
    chat: gramdrive_model::identity::ChatKey,
    year: u16,
    month: u8,
) -> ItemId {
    ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::MonthDir(MonthDirKey { chat, year, month }),
    })
    .id()
}

fn insert_date_first_directory(
    tx: &Transaction<'_>,
    id: ItemId,
    parent: ItemId,
    name: &str,
) -> Result<(), StateError> {
    let ItemKey::Appearance(AppearanceKey { view, item }) = id.key() else {
        return Err(StateError::InvalidArgument {
            what: "date-first directory must be a chat-list appearance",
        });
    };
    let (scope, kind) = match item {
        CanonicalKey::ActiveStories(key) => (key.chat.scope, "active_stories"),
        CanonicalKey::MonthDir(key) => (key.chat.scope, "month_dir"),
        _ => {
            return Err(StateError::InvalidArgument {
                what: "unsupported date-first directory kind",
            });
        }
    };
    let (view_kind, folder_id) = match view {
        ChatListKind::Main => ("main", None),
        ChatListKind::Archive => ("archive", None),
        ChatListKind::Stories => ("stories", None),
        ChatListKind::Folder(folder) => ("folder", Some(i64::from(folder.0))),
    };
    let canonical = ItemKey::Canonical(item).id();
    tx.execute(
        "INSERT INTO items (
             item_id, account_id, namespace_version, kind, parent_item_id,
             canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
             is_directory, metadata_version, availability
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 1,
                   'date-first-v1', 'fetchable')
         ON CONFLICT (item_id) DO UPDATE SET
             parent_item_id = excluded.parent_item_id,
             display_name = excluded.display_name,
             safe_name = excluded.safe_name,
             deleted_at_ms = NULL,
             metadata_version = excluded.metadata_version",
        params![
            id.as_bytes(),
            scope.account.account_id.0,
            i64::from(scope.namespace_version.0),
            kind,
            parent.as_bytes(),
            canonical.as_bytes(),
            view_kind,
            folder_id,
            name,
        ],
    )?;
    Ok(())
}

fn insert_date_first_document(
    tx: &Transaction<'_>,
    view: ChatListKind,
    chat: gramdrive_model::identity::ChatKey,
    parent: ItemId,
    year: u16,
    month: u8,
    format: DocFormat,
) -> Result<(), StateError> {
    let canonical_key = CanonicalKey::GeneratedDoc(gramdrive_model::identity::GeneratedDocKey {
        chat,
        partition: DocPartition::Month { year, month },
        format,
        schema_family: SchemaFamily(1),
    });
    let id = ItemKey::Appearance(AppearanceKey {
        view,
        item: canonical_key,
    })
    .id();
    let canonical = ItemKey::Canonical(canonical_key).id();
    let (view_kind, folder_id) = match view {
        ChatListKind::Main => ("main", None),
        ChatListKind::Archive => ("archive", None),
        ChatListKind::Stories => ("stories", None),
        ChatListKind::Folder(folder) => ("folder", Some(i64::from(folder.0))),
    };
    let (name, mime) = match format {
        DocFormat::Markdown => ("Messages.md", "text/markdown"),
        DocFormat::Ndjson => ("Messages.ndjson", "application/x-ndjson"),
        DocFormat::Json => {
            return Err(StateError::InvalidArgument {
                what: "month contract only creates Markdown and NDJSON documents",
            });
        }
    };
    tx.execute(
        "INSERT INTO items (
             item_id, account_id, namespace_version, kind, parent_item_id,
             canonical_item_id, view_kind, view_folder_id, display_name, safe_name,
             is_directory, mime_type, metadata_version, content_version, availability
         ) VALUES (?1, ?2, ?3, 'generated_doc', ?4, ?5, ?6, ?7, ?8, ?8,
                   0, ?9, 'date-first-v1', 'unrendered-date-first-v1', 'fetchable')
         ON CONFLICT (item_id) DO UPDATE SET
             parent_item_id = excluded.parent_item_id,
             display_name = excluded.display_name,
             safe_name = excluded.safe_name,
             mime_type = excluded.mime_type,
             deleted_at_ms = NULL,
             metadata_version = excluded.metadata_version",
        params![
            id.as_bytes(),
            chat.scope.account.account_id.0,
            i64::from(chat.scope.namespace_version.0),
            parent.as_bytes(),
            canonical.as_bytes(),
            view_kind,
            folder_id,
            name,
            mime,
        ],
    )?;
    Ok(())
}

/// The chunk loop. Every path out of it leaves the database consistent with
/// its version: either the version is old and the checkpoint says where to
/// resume, or the version is new and there is no checkpoint.
fn apply_resumable(
    conn: &mut Connection,
    migration: &Migration,
    prepare: Option<&'static str>,
    chunk: ChunkFn,
) -> Result<(), StateError> {
    loop {
        let tx = conn.transaction()?;
        let checkpoint = read_checkpoint(&tx, migration.version)?;
        if checkpoint.is_none() {
            // No committed chunk yet, so the preamble either has never run
            // or was rolled back with the chunk that would have committed it.
            if let Some(sql) = prepare {
                tx.execute_batch(sql)?;
            }
        }

        match chunk(&tx, checkpoint.as_deref())? {
            ChunkOutcome::More { checkpoint: next } => {
                if checkpoint.as_deref() == Some(next.as_str()) {
                    return Err(StateError::MigrationStalled { checkpoint: next });
                }
                save_checkpoint(&tx, migration, &next)?;
                // Raised with the first checkpoint that commits and cleared
                // by the transaction that finishes the migration: the marker
                // is durable for exactly as long as an unfinished tail is.
                repair::raise(
                    &tx,
                    RepairKind::MigrationInterrupted,
                    &interrupted(migration),
                )?;
                tx.commit()?;
            }
            ChunkOutcome::Done => {
                clear_checkpoint(&tx, migration.version)?;
                repair::clear(
                    &tx,
                    RepairKind::MigrationInterrupted,
                    &interrupted(migration),
                )?;
                finish(&tx, migration)?;
                tx.commit()?;
                return Ok(());
            }
        }
    }
}

/// Records the migration and stamps the version. Called inside the
/// transaction that carries the migration's last piece of work — the stamp
/// and the work it describes commit together or not at all.
fn finish(tx: &Transaction<'_>, migration: &Migration) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO schema_history (version, applied_at_ms) VALUES (?1, unixepoch() * 1000)",
        [migration.version],
    )?;
    tx.pragma_update(None, "user_version", migration.version)?;
    Ok(())
}

/// The `detail` half of a migration's [`RepairKind::MigrationInterrupted`]
/// marker identity. Stable per migration, so a resume re-raises the same
/// marker instead of a second one.
fn interrupted(migration: &Migration) -> String {
    format!("migration {} ({})", migration.version, migration.name)
}

fn current_version(conn: &Connection) -> Result<i64, StateError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn read_checkpoint(tx: &Transaction<'_>, version: i64) -> Result<Option<String>, StateError> {
    Ok(tx
        .query_row(
            "SELECT checkpoint FROM migration_progress WHERE version = ?1",
            [version],
            |row| row.get(0),
        )
        .optional()?)
}

fn save_checkpoint(
    tx: &Transaction<'_>,
    migration: &Migration,
    checkpoint: &str,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO migration_progress
             (version, name, checkpoint, chunks_done, started_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 1, unixepoch() * 1000, unixepoch() * 1000)
         ON CONFLICT (version) DO UPDATE SET
             checkpoint    = excluded.checkpoint,
             chunks_done   = chunks_done + 1,
             updated_at_ms = excluded.updated_at_ms",
        params![migration.version, migration.name, checkpoint],
    )?;
    Ok(())
}

fn clear_checkpoint(tx: &Transaction<'_>, version: i64) -> Result<(), StateError> {
    tx.execute(
        "DELETE FROM migration_progress WHERE version = ?1",
        [version],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The runner is exercised against a real v1 fixture database with a
    //! migration that does the thing this framework exists for: a schema
    //! change plus a data backfill too big for one transaction.
    //!
    //! Most migrations here are test-only, targeting the runner's own
    //! mechanics (chunking, checkpoints, interruption, resume) with a
    //! resumable shape the shipped registry does not have yet. The shipped
    //! [`MIGRATIONS`] are applied to the same v1 fixture in their own test
    //! below.

    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rusqlite::Connection;

    use super::*;

    /// Representative rows of a v1 database — see the file for what is in it
    /// and why.
    const V1_SEED_SQL: &str = include_str!("../fixtures/v1_seed.sql");

    /// Messages in the fixture. The chunk size below divides it into several
    /// chunks: a "resumable" migration that resumes exactly once proves less
    /// than one that can be interrupted in the middle of a run.
    const FIXTURE_MESSAGES: usize = 12;

    /// Rows one chunk of [`fill_render_hint`] handles.
    const CHUNK_ROWS: usize = 4;

    /// The version the test migrations produce.
    const V2: i64 = 2;

    thread_local! {
        /// How many chunks to let through before injecting a failure, or
        /// `None` to let the migration finish.
        static FAIL_AFTER_CHUNKS: Cell<Option<u32>> = const { Cell::new(None) };
        /// Chunks [`fill_render_hint`] has committed in this test.
        static CHUNKS_RUN: Cell<u32> = const { Cell::new(0) };
    }

    fn arm_failure_after(chunks: u32) {
        FAIL_AFTER_CHUNKS.with(|cell| cell.set(Some(chunks)));
        CHUNKS_RUN.with(|cell| cell.set(0));
    }

    fn disarm_failure() {
        FAIL_AFTER_CHUNKS.with(|cell| cell.set(None));
    }

    /// A realistic v2: add a column to the `messages` projection and fill it
    /// for every existing row. The fill cannot be one transaction on a real
    /// account (110k messages), so it is chunked, and the `ALTER TABLE` its
    /// chunks depend on is the `prepare` preamble.
    const RENDER_HINT: &[Migration] = &[Migration {
        version: V2,
        name: "messages_render_hint",
        step: MigrationStep::Resumable {
            prepare: Some("ALTER TABLE messages ADD COLUMN render_hint TEXT"),
            chunk: fill_render_hint,
        },
    }];

    /// The same version done as one transaction — the shape every migration
    /// small enough to fit should use.
    const RENDER_HINT_ATOMIC: &[Migration] = &[Migration {
        version: V2,
        name: "messages_render_hint_atomic",
        step: MigrationStep::Sql(
            "ALTER TABLE messages ADD COLUMN render_hint TEXT;
             UPDATE messages SET render_hint = 'hint-' || chat_id || '-' || message_id;",
        ),
    }];

    const INVALID_FK_ATOMIC: &[Migration] = &[Migration {
        version: V2,
        name: "invalid_fk_atomic",
        step: MigrationStep::AtomicRebuild(insert_orphan_chat),
    }];

    fn insert_orphan_chat(tx: &Transaction<'_>) -> Result<(), StateError> {
        tx.execute(
            "INSERT INTO chats (account_id, namespace_version, chat_id, chat_type,
                                 title, metadata_version)
             VALUES (999999, 1, 1, 'private', 'Orphan', 'bad')",
            [],
        )?;
        Ok(())
    }

    /// Fills `render_hint` for [`CHUNK_ROWS`] messages per chunk, resuming
    /// after the last `latest_event_seq` it committed.
    ///
    /// `latest_event_seq` is the cursor because it is unique per message and
    /// indexed — a chunked migration whose cursor needs a table scan to
    /// resume has only moved the cost around.
    fn fill_render_hint(
        tx: &Transaction<'_>,
        checkpoint: Option<&str>,
    ) -> Result<ChunkOutcome, StateError> {
        let after: i64 = checkpoint.map_or(0, |text| {
            text.parse()
                .expect("the runner returns its own checkpoints")
        });

        if let Some(limit) = FAIL_AFTER_CHUNKS.with(Cell::get)
            && CHUNKS_RUN.with(Cell::get) >= limit
        {
            // A genuine database error inside a chunk. The durable state it
            // leaves is the state a process kill would leave: committed
            // chunks on disk, this transaction rolled back.
            tx.execute_batch("SELECT 1 FROM a_table_that_does_not_exist")?;
        }

        let mut statement = tx.prepare(
            "SELECT chat_id, message_id, latest_event_seq FROM messages
             WHERE latest_event_seq > ?1
             ORDER BY latest_event_seq
             LIMIT ?2",
        )?;
        let rows: Vec<(i64, i64, i64)> = statement
            .query_map(params![after, CHUNK_ROWS as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;

        let Some(&(_, _, last_seq)) = rows.last() else {
            return Ok(ChunkOutcome::Done);
        };

        for (chat_id, message_id, _) in &rows {
            tx.execute(
                "UPDATE messages SET render_hint = 'hint-' || chat_id || '-' || message_id
                 WHERE account_id = 7 AND namespace_version = 1
                   AND chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id],
            )?;
        }

        CHUNKS_RUN.with(|cell| cell.set(cell.get() + 1));
        Ok(ChunkOutcome::More {
            checkpoint: last_seq.to_string(),
        })
    }

    /// A chunk that always asks to be called again with what it was given.
    fn never_progresses(
        _tx: &Transaction<'_>,
        checkpoint: Option<&str>,
    ) -> Result<ChunkOutcome, StateError> {
        Ok(ChunkOutcome::More {
            checkpoint: checkpoint.unwrap_or("stuck").to_owned(),
        })
    }

    const STALLING: &[Migration] = &[Migration {
        version: V2,
        name: "stalls_forever",
        step: MigrationStep::Resumable {
            prepare: None,
            chunk: never_progresses,
        },
    }];

    /// A unique database path under the OS temp directory, cleaned on drop.
    /// Uniqueness from process id and a counter — no clock, no randomness.
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gramdrive-migrate-test-{}-{n}.sqlite3",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self { path }
        }

        /// Opens the file as a v1 fixture database: baseline schema, journal,
        /// representative rows.
        fn open_v1(&self) -> Connection {
            let mut conn = Connection::open(&self.path).expect("open");
            seed_v1(&mut conn);
            conn
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

    /// Brings `conn` to the v1 fixture state: the frozen baseline plus the
    /// runner's journal — exactly what a database created by the v1 build
    /// looks like — then the seed rows, with foreign keys on so the fixture
    /// cannot claim rows the schema would reject. Deliberately *not*
    /// `ensure_schema`, which would migrate the fixture past the version
    /// these tests exist to start from.
    fn seed_v1(conn: &mut Connection) {
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        crate::schema::apply_baseline(conn).expect("baseline schema");
        ensure_journal(conn).expect("journal");
        conn.execute_batch(V1_SEED_SQL).expect("v1 seed rows");
    }

    fn memory_v1() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in memory");
        seed_v1(&mut conn);
        conn
    }

    fn version_of(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    }

    /// Every message and the hint the migration gave it, in a stable order.
    fn render_hints(conn: &Connection) -> Vec<(i64, i64, Option<String>)> {
        let mut statement = conn
            .prepare(
                "SELECT chat_id, message_id, render_hint FROM messages
                 ORDER BY chat_id, message_id",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query");
        rows.collect::<Result<_, _>>().expect("rows")
    }

    fn checkpoint_row(conn: &Connection) -> Option<(String, i64)> {
        conn.query_row(
            "SELECT checkpoint, chunks_done FROM migration_progress WHERE version = ?1",
            [V2],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("checkpoint query")
    }

    fn marker_details(conn: &Connection) -> Vec<String> {
        repair::list(conn)
            .expect("markers")
            .into_iter()
            .filter(|marker| marker.kind == RepairKind::MigrationInterrupted)
            .map(|marker| marker.detail)
            .collect()
    }

    // --- The registry contract -------------------------------------------

    #[test]
    fn shipped_registry_agrees_with_the_schema_version() {
        // The const assertion above is the real gate — this fails the same
        // way at runtime, and names what the compile error means.
        assert_eq!(
            MIGRATIONS.len() as i64,
            SCHEMA_VERSION - BASELINE_VERSION,
            "every version above the baseline needs exactly one migration"
        );
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                BASELINE_VERSION + 1 + index as i64,
                "migrations must be contiguous and ascending"
            );
        }
    }

    #[test]
    fn every_migration_ships_a_fixture_of_the_schema_it_migrates_from() {
        // The AC this framework is built around. Vacuous today (no
        // migrations), and deliberately so: it fails the moment someone adds
        // a migration without the fixture database that proves it against
        // the schema it will actually meet in the field.
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        for migration in MIGRATIONS {
            let prior = migration.version - 1;
            let seed = fixtures.join(format!("v{prior}_seed.sql"));
            assert!(
                seed.is_file(),
                "migration {} ({}) has no fixture for the schema it migrates from: \
                 expected {}",
                migration.version,
                migration.name,
                seed.display()
            );
        }
    }

    #[test]
    fn v22_installs_the_auth_finalization_decision_journal_atomically() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 21).expect("migrate installed database to v21");
        let before: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'auth_finalization_journal'",
                [],
                |row| row.get(0),
            )
            .expect("table probe");
        assert_eq!(before, 0);

        run(&mut conn, MIGRATIONS, 22).expect("migrate installed database to v22");
        conn.execute(
            "INSERT INTO auth_finalization_journal (
                 account_id, phase, had_account_row, had_database_key, had_tdlib_state
             ) VALUES (777000123, 'prepared', 1, 1, 1)",
            [],
        )
        .expect("prepared decision");
        conn.execute(
            "UPDATE auth_finalization_journal SET phase = 'committed'
             WHERE account_id = 777000123",
            [],
        )
        .expect("commit decision");
        let phase: String = conn
            .query_row(
                "SELECT phase FROM auth_finalization_journal WHERE account_id = 777000123",
                [],
                |row| row.get(0),
            )
            .expect("decision read");
        assert_eq!(phase, "committed");
        assert_eq!(version_of(&conn), 22);
    }

    #[test]
    fn v8_redacts_legacy_protected_story_rows_and_seeds_resumable_work() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 7).expect("migrate fixture to v7");
        conn.execute(
            "INSERT INTO stories (
                 account_id, namespace_version, poster_chat_id, story_id,
                 source_timestamp_ms, mime_type, exact_size, content_version,
                 availability, can_be_forwarded)
             VALUES (7, 1, 100, 91, 1704067200000, 'secret/locator', 999,
                     'legacy-protected-v7', 'restricted', 0)",
            [],
        )
        .expect("legacy protected row");

        run(&mut conn, MIGRATIONS, 8).expect("migrate fixture to v8");
        let row: (String, Option<String>, Option<i64>, String, bool, String) = conn
            .query_row(
                "SELECT content_state, mime_type, exact_size, availability,
                        can_be_forwarded, content_version
                 FROM stories WHERE account_id = 7 AND namespace_version = 1
                   AND poster_chat_id = 100 AND story_id = 91",
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
            .expect("migrated story");
        assert_eq!(
            row,
            (
                "protected".to_owned(),
                None,
                None,
                "restricted".to_owned(),
                false,
                "story-protected/100/91".to_owned(),
            )
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM story_sync_progress", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("seeded progress"),
            3
        );
        assert!(
            conn.execute(
                "UPDATE stories SET mime_type = 'must-not-persist'
                 WHERE account_id = 7 AND namespace_version = 1
                   AND poster_chat_id = 100 AND story_id = 91",
                [],
            )
            .is_err(),
            "schema guard must reject protected metadata bypasses"
        );
    }

    #[test]
    fn the_v1_fixture_is_a_real_v1_database() {
        let conn = memory_v1();
        assert_eq!(version_of(&conn), BASELINE_VERSION);

        let messages: usize = conn
            .query_row("SELECT count(*) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .expect("count");
        assert_eq!(messages, FIXTURE_MESSAGES);

        // Foreign keys were on while it loaded, so this is not a fixture
        // that only looks like a v1 database.
        let violations: usize = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare")
            .query_map([], |_| Ok(()))
            .expect("query")
            .count();
        assert_eq!(violations, 0);
    }

    // --- The shipped registry against the fixture --------------------------

    #[test]
    fn the_shipped_v2_migration_creates_the_item_change_journal() {
        let mut conn = memory_v1();

        run(&mut conn, MIGRATIONS, SCHEMA_VERSION).expect("migrate the v1 fixture");

        assert_eq!(version_of(&conn), SCHEMA_VERSION);
        let instance: String = conn
            .query_row(
                "SELECT instance_id FROM item_change_journal WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("the journal identity row");
        assert_eq!(
            instance.len(),
            32,
            "a 16-byte random identity in lowercase hex"
        );
        let changes: i64 = conn
            .query_row("SELECT count(*) FROM item_changes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(
            changes, 0,
            "no backfill: items that predate the journal have no changes to report"
        );
    }

    #[test]
    fn v15_renames_chat_metadata_in_place_and_journals_the_installed_item() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 14).expect("migrate fixture to v14");
        conn.execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, metadata_version
             ) VALUES (
                 X'F1', 7, 1, 'account', NULL, NULL, NULL, 'Account', 'Account',
                 1, 'account-v14'
             );
             INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, mime_type, logical_size, metadata_version,
                 content_version
             ) VALUES (
                 X'F2', 7, 1, 'generated_doc', X'F1', X'F3', 'main',
                 'chat.json', 'chat.json', 0, 'application/json', 17,
                 'chat-metadata-v14', 'chat-content-v1'
             );
             INSERT INTO cache_entries (
                 item_id, account_id, content_version, kind, size,
                 verification, pinned, last_access_at_ms, materialized_at_ms,
                 materialization_ref
             ) VALUES (
                 X'F2', 7, 'chat-content-v1', 'generated_doc', 17,
                 'verified', 0, 10, 10, '/cache/current/chat.json'
             );",
        )
        .expect("seed installed v14 chat metadata");

        run(&mut conn, MIGRATIONS, 15).expect("migrate fixture to v15");

        let item: (Vec<u8>, String, String, String) = conn
            .query_row(
                "SELECT item_id, display_name, safe_name, metadata_version
                 FROM items WHERE item_id = X'F2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("migrated item");
        assert_eq!(
            item,
            (
                vec![0xF2],
                ".chat.json".to_owned(),
                ".chat.json".to_owned(),
                "hidden-chat-metadata-v15".to_owned(),
            ),
            "the provider identity is unchanged while the filename becomes hidden"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM items
                 WHERE deleted_at_ms IS NULL
                   AND (display_name = 'chat.json' OR safe_name = 'chat.json')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("legacy-name count"),
            0,
            "the migration leaves no visible chat.json orphan"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM item_changes WHERE item_id = X'F2'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("journal count"),
            1,
            "the installed provider receives the in-place rename"
        );
        assert_eq!(
            conn.query_row(
                "SELECT materialization_ref FROM cache_entries WHERE item_id = X'F2'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("cache reference"),
            "/cache/current/chat.json",
            "existing generated bytes stay attached to the stable item"
        );
    }

    #[test]
    fn v16_rolls_up_installed_directory_sizes_without_touching_identity() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 15).expect("migrate fixture to v15");
        // An already-installed namespace: chat -> month -> two documents and
        // one attachment, plus a hidden chat-metadata document directly under
        // the chat, plus a tombstoned attachment that must not be counted.
        conn.execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, metadata_version
             ) VALUES
                 (X'A1', 7, 1, 'account', NULL, NULL, NULL, 'Account', 'Account',
                  1, 'account-v15'),
                 (X'A2', 7, 1, 'chat', X'A1', X'B2', 'main', 'Chat', 'Chat',
                  1, 'chat-v15'),
                 (X'A3', 7, 1, 'month_dir', X'A2', X'B3', 'main', '2026-07',
                  '2026-07', 1, 'month-v15');
             INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, logical_size, metadata_version
             ) VALUES
                 (X'A4', 7, 1, 'generated_doc', X'A3', X'B4', 'main',
                  'Messages.md', 'Messages.md', 0, 100, 'md-v15'),
                 (X'A5', 7, 1, 'generated_doc', X'A3', X'B5', 'main',
                  'Messages.ndjson', 'Messages.ndjson', 0, 250, 'ndjson-v15'),
                 (X'A6', 7, 1, 'attachment', X'A3', X'B6', 'main',
                  'photo.jpg', 'photo.jpg', 0, 4000, 'attachment-v15'),
                 (X'A8', 7, 1, 'generated_doc', X'A2', X'B8', 'main',
                  '.chat.json', '.chat.json', 0, 17, 'chat-json-v15');
             INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, logical_size, metadata_version, deleted_at_ms
             ) VALUES
                 (X'A7', 7, 1, 'attachment', X'A3', X'B7', 'main',
                  'gone.jpg', 'gone.jpg', 0, 999999, 'attachment-v15', 50);",
        )
        .expect("seed an installed v15 namespace");

        run(&mut conn, MIGRATIONS, 16).expect("migrate fixture to v16");

        let rollup = |id: u8| -> Option<i64> {
            conn.query_row(
                "SELECT aggregate_size FROM items WHERE item_id = ?1",
                params![vec![id]],
                |row| row.get(0),
            )
            .expect("rollup")
        };
        assert_eq!(
            rollup(0xA3),
            Some(4350),
            "a month sums exactly its live children: 100 + 250 + 4000"
        );
        assert_eq!(
            rollup(0xA2),
            Some(4367),
            "a chat sums its months plus its own direct files, and never \
             counts a tombstoned descendant"
        );
        assert_eq!(rollup(0xA6), None, "a file carries no descendant rollup");

        type IdentityRow = (Vec<u8>, Option<Vec<u8>>, String, String);
        let identity: Vec<IdentityRow> = conn
            .prepare(
                "SELECT item_id, parent_item_id, safe_name, metadata_version
                 FROM items ORDER BY item_id",
            )
            .expect("prepare")
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            identity.len(),
            8,
            "no row is added or removed by the rollup"
        );
        assert!(
            identity
                .iter()
                .all(|(_, _, _, version)| version.ends_with("-v15")),
            "identifiers, parents, names and versions are untouched: the \
             migration only fills a new column"
        );

        let journalled: Vec<Vec<u8>> = conn
            .prepare("SELECT item_id FROM item_changes ORDER BY item_id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            journalled,
            vec![vec![0xA2], vec![0xA3]],
            "an installed provider is told about exactly the directories \
             whose published size just appeared"
        );
    }

    #[test]
    fn v16_sums_a_namespace_with_nothing_to_sum_and_claims_nothing_above_a_chat() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 15).expect("migrate fixture to v15");
        conn.execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, metadata_version
             ) VALUES
                 (X'C1', 7, 1, 'account', NULL, NULL, NULL, 'Account', 'Account',
                  1, 'account-v15'),
                 (X'C3', 7, 1, 'chat_list', X'C1', X'D3', 'main', 'Chats', 'Chats',
                  1, 'list-v15'),
                 (X'C2', 7, 1, 'chat', X'C3', X'D2', 'main', 'Empty', 'Empty',
                  1, 'chat-v15');",
        )
        .expect("seed an empty installed chat");

        run(&mut conn, MIGRATIONS, 16).expect("migrate fixture to v16");

        let rollup = |id: u8| -> Option<i64> {
            conn.query_row(
                "SELECT aggregate_size FROM items WHERE item_id = ?1",
                params![vec![id]],
                |row| row.get(0),
            )
            .expect("rollup")
        };
        assert_eq!(
            rollup(0xC2),
            Some(0),
            "a chat with no indexed descendants publishes zero, not an unknown"
        );
        // The kinds above a chat own no rollup at all. `NULL` and `0` are
        // different claims — "nothing is asserted here" against "this
        // subtree is indexed and empty" — and the projection agrees with
        // this SQL rather than filling a zero of its own (BUG-260728-2qfzbd).
        assert_eq!(
            rollup(0xC3),
            None,
            "a chat list holds chats, not correspondence: it claims no size"
        );
        assert_eq!(rollup(0xC1), None, "the account root claims no size either");
    }

    #[test]
    fn v16_rollup_is_idempotent_when_the_backfill_runs_a_second_time() {
        // The migration runner applies v16 once, so re-running the whole
        // migration is not what this asserts. What it asserts is that the
        // backfill *statements* are a fixed point: replaying them over an
        // already-migrated namespace produces byte-identical rollups. That
        // is the property a re-entrant or retried migration depends on.
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 15).expect("migrate fixture to v15");
        conn.execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, metadata_version
             ) VALUES
                 (X'E1', 7, 1, 'account', NULL, NULL, NULL, 'Account', 'Account',
                  1, 'account-v15'),
                 (X'E2', 7, 1, 'chat', X'E1', X'F2', 'main', 'Chat', 'Chat',
                  1, 'chat-v15'),
                 (X'E3', 7, 1, 'month_dir', X'E2', X'F3', 'main', '2026-07',
                  '2026-07', 1, 'month-v15');
             INSERT INTO items (
                 item_id, account_id, namespace_version, kind, parent_item_id,
                 canonical_item_id, view_kind, display_name, safe_name,
                 is_directory, logical_size, metadata_version
             ) VALUES
                 (X'E4', 7, 1, 'attachment', X'E3', X'F4', 'main',
                  'photo.jpg', 'photo.jpg', 0, 4096, 'attachment-v15');",
        )
        .expect("seed an installed v15 namespace");

        run(&mut conn, MIGRATIONS, 16).expect("migrate fixture to v16");

        let snapshot = |conn: &rusqlite::Connection| -> Vec<(Vec<u8>, Option<i64>, String)> {
            conn.prepare(
                "SELECT item_id, aggregate_size, metadata_version
                 FROM items ORDER BY item_id",
            )
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
        };
        let first = snapshot(&conn);
        assert_eq!(
            first
                .iter()
                .find(|(id, _, _)| id.as_slice() == [0xE2])
                .map(|(_, size, _)| *size),
            Some(Some(4096)),
            "the chat rolls up its month's only attachment"
        );

        // Replay the backfill statements alone, exactly as the migration
        // runs them, minus the one-time ALTER TABLE.
        let backfill: String = include_str!("schema/v16.sql")
            .lines()
            .filter(|line| {
                !line
                    .trim_start()
                    .starts_with("ALTER TABLE items ADD COLUMN")
            })
            .collect::<Vec<_>>()
            .join("\n");
        conn.execute_batch(&backfill)
            .expect("replay the v16 backfill");

        assert_eq!(
            snapshot(&conn),
            first,
            "replaying the backfill changes no rollup and no version: the \
             statements are a fixed point, so a retried migration converges"
        );
    }

    #[test]
    fn v17_gives_every_starved_chat_one_guaranteed_turn_without_losing_cursors() {
        // The repair an installed profile needs: chats whose backward crawl
        // was starved by live traffic must lead the rotation exactly once,
        // and no cursor, completion flag, or observation stamp may move
        // doing it (BUG-260728-2qfzbd).
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 16).expect("migrate fixture to v16");
        conn.execute_batch(
            // Chat 100 is the busy one: live delivery kept stamping its
            // observation time, which under the old ordering was also its
            // place in the queue. Chat 200 is quiet and far older.
            "UPDATE chat_sync_state
             SET oldest_loaded_message_id = 10, newest_loaded_message_id = 900,
                 history_complete = 0, last_sync_at_ms = 9000
             WHERE chat_id = 100;
             UPDATE chat_sync_state
             SET oldest_loaded_message_id = 10, newest_loaded_message_id = 20,
                 history_complete = 0, last_sync_at_ms = 10
             WHERE chat_id = 200;",
        )
        .expect("seed a v16 profile whose busy chat was demoted by traffic");

        run(&mut conn, MIGRATIONS, 17).expect("migrate fixture to v17");

        let row = |chat: i64| -> (Option<i64>, Option<i64>, i64, i64) {
            conn.query_row(
                "SELECT last_backfill_at_ms, last_sync_at_ms,
                        oldest_loaded_message_id, newest_loaded_message_id
                 FROM chat_sync_state WHERE chat_id = ?1",
                params![chat],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("cursor row")
        };
        assert_eq!(
            row(100),
            (None, Some(9000), 10, 900),
            "the busy chat starts with no turn taken — it leads the rotation \
             — while its cursor and observation stamp are untouched"
        );
        assert_eq!(
            row(200),
            (None, Some(10), 10, 20),
            "so does the quiet one: the repair is one guaranteed turn each, \
             not a reordering that favours either"
        );

        // The index the backlog reads must exist and the one nothing reads
        // any more must be gone, or the fix trades starvation for a scan.
        let index_names: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND tbl_name = 'chat_sync_state'
                 ORDER BY name",
            )
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert!(
            index_names
                .iter()
                .any(|n| n == "chat_sync_state_backfill_turns"),
            "the rotation key needs its index, got {index_names:?}"
        );
        assert!(
            !index_names.iter().any(|n| n == "chat_sync_state_backlog"),
            "the old key's index has no reader left and is maintained on \
             every live message, got {index_names:?}"
        );
    }

    #[test]
    fn v18_backfills_every_tombstone_and_enforces_reason_codes() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 17).expect("migrate fixture to v17");
        conn.execute_batch(
            "INSERT INTO items (
                 item_id, account_id, namespace_version, kind, display_name,
                 safe_name, is_directory, metadata_version, availability,
                 deleted_at_ms)
             VALUES
                 (X'A1', 7, 1, 'account', 'legacy-a', 'legacy-a', 1,
                  'retention-purge-7-1', 'fetchable', 1000),
                 (X'A2', 7, 1, 'account', 'legacy-b', 'legacy-b', 1,
                  'projection-v1', 'fetchable', 2000);",
        )
        .expect("seed v17 tombstones without provenance");

        run(&mut conn, MIGRATIONS, 18).expect("migrate fixture to v18");
        let provenance = |id: &[u8]| -> String {
            conn.query_row(
                "SELECT tombstone_provenance FROM items WHERE item_id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("provenance")
        };
        assert_eq!(provenance(&[0xA1]), "retention");
        assert_eq!(provenance(&[0xA2]), "reconcile");

        let missing = conn.execute(
            "UPDATE items SET tombstone_provenance = NULL WHERE item_id = X'A2'",
            [],
        );
        assert!(missing.is_err(), "a tombstone cannot lose provenance");
        let unknown = conn.execute(
            "UPDATE items SET tombstone_provenance = 'free-form' WHERE item_id = X'A2'",
            [],
        );
        assert!(
            unknown.is_err(),
            "provenance is a fixed privacy-safe vocabulary"
        );
    }

    #[test]
    fn v19_adds_durable_policy_skip_bookkeeping_to_an_installed_v18_database() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 18).expect("migrate fixture to v18");
        let columns = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT name FROM pragma_table_info('render_state')")
                .expect("prepare render-state columns")
                .query_map([], |row| row.get(0))
                .expect("query render-state columns")
                .collect::<Result<_, _>>()
                .expect("render-state column rows")
        };
        assert!(!columns(&conn).contains(&"skip_reason".to_owned()));

        run(&mut conn, MIGRATIONS, 19).expect("migrate installed v18 database to v19");
        assert_eq!(version_of(&conn), 19);
        let columns = columns(&conn);
        assert!(columns.contains(&"skip_reason".to_owned()));
        assert!(columns.contains(&"skipped_at_ms".to_owned()));
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_list('render_state')")
            .expect("prepare render-state indexes")
            .query_map([], |row| row.get(0))
            .expect("query render-state indexes")
            .collect::<Result<_, _>>()
            .expect("render-state index rows");
        assert!(
            indexes.contains(&"render_state_policy_excluded".to_owned()),
            "installed databases retain an index for aggregate excluded-work accounting"
        );
    }

    #[test]
    fn v20_indexes_live_generated_documents_by_parent_on_installed_profiles() {
        let mut conn = memory_v1();
        run(&mut conn, MIGRATIONS, 19).expect("migrate fixture to v19");

        let indexes = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT name FROM pragma_index_list('items')")
                .expect("prepare items indexes")
                .query_map([], |row| row.get(0))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("index rows")
        };
        assert!(!indexes(&conn).contains(&"items_live_generated_docs_by_parent".to_owned()));

        run(&mut conn, MIGRATIONS, 20).expect("migrate installed v19 database to v20");
        assert_eq!(version_of(&conn), 20);
        assert!(
            indexes(&conn).contains(&"items_live_generated_docs_by_parent".to_owned()),
            "installed databases index the direct live generated-document children that chat rendering reads"
        );
    }

    // --- Applying a migration ---------------------------------------------

    #[test]
    fn atomic_migration_advances_the_version_and_records_history() {
        let mut conn = memory_v1();

        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("migrate");

        assert_eq!(version_of(&conn), V2);
        let history: Vec<i64> = conn
            .prepare("SELECT version FROM schema_history ORDER BY version")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(history, vec![BASELINE_VERSION, V2]);
        assert!(
            render_hints(&conn)
                .iter()
                .all(|(_, _, hint)| hint.is_some()),
            "every row should carry a hint"
        );
        assert_eq!(
            checkpoint_row(&conn),
            None,
            "an atomic migration leaves no checkpoint"
        );
    }

    #[test]
    fn atomic_rebuild_rolls_back_before_stamping_on_foreign_key_violation() {
        let mut conn = memory_v1();

        let error = run(&mut conn, INVALID_FK_ATOMIC, V2).expect_err("invalid rebuild");

        assert!(
            matches!(error, StateError::MigrationFailed { version: V2, .. }),
            "expected a named migration failure, got {error:?}"
        );
        assert_eq!(version_of(&conn), BASELINE_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM schema_history WHERE version = ?1",
                [V2],
                |row| row.get::<_, i64>(0),
            )
            .expect("history count"),
            0,
            "the rejected version must not be stamped"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM chats WHERE account_id = 999999",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("orphan count"),
            0,
            "the violating rebuild writes must roll back"
        );
        assert_eq!(
            conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .expect("foreign_keys"),
            1,
            "foreign-key enforcement must be restored after rejection"
        );
    }

    #[test]
    fn a_database_already_at_the_target_is_untouched() {
        let mut conn = memory_v1();
        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("migrate");
        let before = render_hints(&conn);

        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("second run");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(render_hints(&conn), before);
        let applications: i64 = conn
            .query_row(
                "SELECT count(*) FROM schema_history WHERE version = ?1",
                [V2],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(applications, 1, "a migration must not be applied twice");
    }

    #[test]
    fn resumable_migration_checkpoints_each_chunk_and_finishes() {
        disarm_failure();
        let mut conn = memory_v1();

        run(&mut conn, RENDER_HINT, V2).expect("migrate");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(
            checkpoint_row(&conn),
            None,
            "a finished migration clears its checkpoint"
        );
        assert_eq!(
            marker_details(&conn),
            Vec::<String>::new(),
            "a finished migration clears its interruption marker"
        );
        assert_eq!(
            CHUNKS_RUN.with(Cell::get) as usize,
            FIXTURE_MESSAGES.div_ceil(CHUNK_ROWS),
            "the fixture should take more than one chunk, or this proves nothing"
        );
        for (chat, message, hint) in render_hints(&conn) {
            assert_eq!(
                hint.as_deref(),
                Some(format!("hint-{chat}-{message}").as_str())
            );
        }
    }

    // --- Interruption and resume (SYNC-072) -------------------------------

    #[test]
    fn an_interrupted_migration_resumes_from_its_checkpoint() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        // Two chunks commit, the third hits a database error. Everything
        // after this drop is what a fresh process finds on disk.
        arm_failure_after(2);
        let error = run(&mut conn, RENDER_HINT, V2).expect_err("chunk three fails");
        assert!(
            matches!(error, StateError::MigrationFailed { version: V2, .. }),
            "expected a named migration failure, got {error:?}"
        );
        drop(conn);

        // A fresh connection to the file: the version never moved, and the
        // checkpoint says where the committed work stopped.
        let conn = Connection::open(&db.path).expect("reopen");
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "an unfinished migration must never advance the version"
        );
        let (checkpoint, chunks_done) = checkpoint_row(&conn).expect("a durable checkpoint");
        assert_eq!(chunks_done, 2);
        let done_before = render_hints(&conn)
            .into_iter()
            .filter(|(_, _, hint)| hint.is_some())
            .count();
        assert_eq!(
            done_before,
            2 * CHUNK_ROWS,
            "exactly the committed chunks survived"
        );
        drop(conn);

        // Resume: the same migration, handed the checkpoint it committed.
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(checkpoint_row(&conn), None);
        assert!(
            checkpoint.parse::<i64>().is_ok(),
            "the checkpoint is the migration's own cursor"
        );
        for (chat, message, hint) in render_hints(&conn) {
            assert_eq!(
                hint.as_deref(),
                Some(format!("hint-{chat}-{message}").as_str())
            );
        }
    }

    #[test]
    fn resuming_produces_exactly_what_an_uninterrupted_run_produces() {
        // Idempotent resume, stated as the property that matters: an
        // interruption must not be observable in the result.
        disarm_failure();
        let mut clean = memory_v1();
        run(&mut clean, RENDER_HINT, V2).expect("clean run");
        let expected = render_hints(&clean);

        let db = TempDb::new();
        let mut conn = db.open_v1();
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted");
        drop(conn);

        // Interrupt the resume too: a migration that survives one crash but
        // not two has not proven anything about crashes.
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted again");
        drop(conn);

        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume to completion");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(
            render_hints(&conn),
            expected,
            "twice-interrupted and never-interrupted must be indistinguishable"
        );
    }

    #[test]
    fn an_interrupted_migration_leaves_a_repair_marker_until_it_completes() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        arm_failure_after(2);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted");
        drop(conn);

        let conn = Connection::open(&db.path).expect("reopen");
        assert_eq!(
            marker_details(&conn),
            vec!["migration 2 (messages_render_hint)".to_owned()],
            "an interrupted migration is durably recorded, naming itself"
        );
        let raised_at = repair::list(&conn).expect("markers")[0].raised_at_ms;
        drop(conn);

        // Resuming re-raises the same marker rather than a second one, and
        // keeps the timestamp of the interruption.
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted again");
        let markers = repair::list(&conn).expect("markers");
        assert_eq!(markers.len(), 1, "one marker per migration, not per crash");
        assert_eq!(
            markers[0].raised_at_ms, raised_at,
            "the marker dates the interruption, not the last time it was noticed"
        );

        disarm_failure();
        run(&mut conn, RENDER_HINT, V2).expect("resume");
        assert_eq!(
            marker_details(&conn),
            Vec::<String>::new(),
            "completing the migration clears it"
        );
    }

    #[test]
    fn the_preamble_survives_a_rollback_and_is_never_applied_twice() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        // Fail inside the very first chunk: the ALTER TABLE ran in that
        // transaction and must roll back with it.
        arm_failure_after(0);
        run(&mut conn, RENDER_HINT, V2).expect_err("first chunk fails");
        drop(conn);

        let conn = Connection::open(&db.path).expect("reopen");
        assert!(
            !message_columns(&conn)
                .iter()
                .any(|name| name == "render_hint"),
            "a rolled-back preamble leaves no column behind"
        );
        assert_eq!(checkpoint_row(&conn), None);
        drop(conn);

        // The resumed run applies the preamble from scratch — and once the
        // first chunk commits, no later resume re-applies it (a second
        // ALTER TABLE would fail with 'duplicate column name').
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted after the preamble committed");
        drop(conn);

        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume past the committed preamble");
        assert_eq!(version_of(&conn), V2);
    }

    fn message_columns(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_info('messages')")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    // --- Refusals ---------------------------------------------------------

    #[test]
    fn a_chunk_that_does_not_move_its_checkpoint_is_refused() {
        let mut conn = memory_v1();

        let error = run(&mut conn, STALLING, V2).expect_err("stalled");

        match error {
            StateError::MigrationFailed {
                version, source, ..
            } => {
                assert_eq!(version, V2);
                assert!(
                    matches!(*source, StateError::MigrationStalled { ref checkpoint } if checkpoint == "stuck"),
                    "expected a stall, got {source:?}"
                );
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "a stalled migration must not claim to have finished"
        );
    }

    #[test]
    fn a_gap_in_the_sequence_is_refused_rather_than_skipped() {
        let mut conn = memory_v1();
        // A registry that jumps to 3: nothing migrates the file out of 1.
        const GAPPED: &[Migration] = &[Migration {
            version: 3,
            name: "unreachable",
            step: MigrationStep::Sql("SELECT 1"),
        }];

        let error = run(&mut conn, GAPPED, 3).expect_err("gap");

        match error {
            StateError::MigrationRequired { found, supported } => {
                assert_eq!(found, BASELINE_VERSION);
                assert_eq!(supported, 3);
            }
            other => panic!("expected MigrationRequired, got {other:?}"),
        }
        assert_eq!(version_of(&conn), BASELINE_VERSION);
    }

    #[test]
    fn a_failing_migration_names_itself() {
        let mut conn = memory_v1();
        const BROKEN: &[Migration] = &[Migration {
            version: V2,
            name: "broken_ddl",
            step: MigrationStep::Sql("ALTER TABLE nope ADD COLUMN x TEXT"),
        }];

        let error = run(&mut conn, BROKEN, V2).expect_err("broken");

        assert!(
            error
                .to_string()
                .starts_with("migration to version 2 (broken_ddl) failed:"),
            "a migration failure must name the migration: {error}"
        );
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "a failed migration leaves the version describing the data that is there"
        );
    }
}
