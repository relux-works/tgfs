//! Canonical chat facts and chat-list membership/order — the source facts
//! POL-1's `order.json` and the app's canonical order regenerate from
//! (DEC-013, SYNC-026).

use std::collections::HashSet;

use gramdrive_model::identity::{ChatId, ChatKey, ChatListKey, ChatListKind};
use gramdrive_model::version::MetadataVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, scope_columns};

/// Telegram chat flavor (`chats.chat_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    /// A one-on-one chat.
    Private,
    /// A basic group.
    Group,
    /// A supergroup.
    Supergroup,
    /// A broadcast channel.
    Channel,
}

impl ChatType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Supergroup => "supergroup",
            Self::Channel => "channel",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "private" => Ok(Self::Private),
            "group" => Ok(Self::Group),
            "supergroup" => Ok(Self::Supergroup),
            "channel" => Ok(Self::Channel),
            other => Err(StateError::CorruptRow {
                table: "chats",
                detail: format!("unknown chat_type '{other}'"),
            }),
        }
    }
}

/// Canonical metadata of one chat in one namespace epoch (domain-model
/// § Chat). Independent of every view: membership and order live in
/// [`ChatListEntry`], presentation in items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    /// The chat's scoped identity.
    pub key: ChatKey,
    /// Telegram chat flavor.
    pub chat_type: ChatType,
    /// Current title, as observed.
    pub title: String,
    /// Public username, if the chat has one.
    pub username: Option<String>,
    /// Telegram's protected-content flag (POL-4).
    pub is_protected: bool,
    /// Per-chat POL-2 Archive-Mode toggle.
    pub archive_mode: bool,
    /// Metadata version of the chat's provider-visible facts (DOM-003).
    pub metadata_version: MetadataVersion,
    /// POL-3 tombstone: when the user left the chat, if observed.
    pub left_at_ms: Option<i64>,
    /// POL-3 tombstone: when the chat's deletion was observed.
    pub deleted_at_ms: Option<i64>,
    /// When the chat's metadata last changed, if known.
    pub last_update_at_ms: Option<i64>,
}

/// Membership and exact position of one chat in one chat list (DEC-013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatListEntry {
    /// The member chat.
    pub chat_id: ChatId,
    /// Telegram's opaque sort position — larger sorts first.
    pub sort_order: i64,
    /// Whether the chat is pinned in this list; pinned sorts before
    /// everything (POL-1).
    pub pinned: bool,
}

/// An identity-free audit record for one authoritative chat-list replacement.
///
/// It deliberately records only list kind, counts and the source's
/// completeness claim. Member ids, titles and account presentation data stay
/// in their owning tables and never enter this diagnostic history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatListCommitAudit {
    /// Live membership count before this replacement.
    pub before_count: u64,
    /// Membership count the completed snapshot committed.
    pub after_count: u64,
    /// Whether the source declared this replacement a complete snapshot.
    pub is_complete: bool,
    /// Observation time supplied by the snapshot owner.
    pub committed_at_ms: i64,
}

/// The `(list_kind, folder_id)` column pair of a chat-list key.
///
/// Folder id 0 is the schema's sentinel for the built-in lists, so a real
/// folder with id 0 is unrepresentable and rejected here (DEC-013).
fn list_columns(kind: ChatListKind) -> Result<(&'static str, i64), StateError> {
    match kind {
        ChatListKind::Main => Ok(("main", 0)),
        ChatListKind::Archive => Ok(("archive", 0)),
        ChatListKind::Stories => Ok(("stories", 0)),
        ChatListKind::Folder(folder) => {
            if folder.0 == 0 {
                Err(StateError::InvalidArgument {
                    what: "folder id 0 is the built-in-list sentinel, not a real folder",
                })
            } else {
                Ok(("folder", i64::from(folder.0)))
            }
        }
    }
}

/// One chat row exactly as stored, before enum and version columns are
/// converted (conversion can fail, and `rusqlite` row mapping cannot carry
/// [`StateError`]).
struct RawChat {
    chat_id: i64,
    chat_type: String,
    title: String,
    username: Option<String>,
    is_protected: bool,
    archive_mode: bool,
    metadata_version: String,
    left_at_ms: Option<i64>,
    deleted_at_ms: Option<i64>,
    last_update_at_ms: Option<i64>,
}

fn read_chat(row: &Row<'_>) -> Result<RawChat, rusqlite::Error> {
    Ok(RawChat {
        chat_id: row.get("chat_id")?,
        chat_type: row.get("chat_type")?,
        title: row.get("title")?,
        username: row.get("username")?,
        is_protected: row.get("is_protected")?,
        archive_mode: row.get("archive_mode")?,
        metadata_version: row.get("metadata_version")?,
        left_at_ms: row.get("left_at_ms")?,
        deleted_at_ms: row.get("deleted_at_ms")?,
        last_update_at_ms: row.get("last_update_at_ms")?,
    })
}

impl ReadTxn<'_> {
    /// The most recent audited snapshot replacement for this list.
    pub fn latest_chat_list_commit_audit(
        &self,
        list: &ChatListKey,
    ) -> Result<Option<ChatListCommitAudit>, StateError> {
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        self.conn()
            .prepare_cached(
                "SELECT before_count, after_count, is_complete, committed_at_ms
                   FROM chat_list_commit_audit
                  WHERE account_id = ?1 AND namespace_version = ?2
                    AND list_kind = ?3 AND folder_id = ?4
                  ORDER BY sequence DESC LIMIT 1",
            )?
            .query_row(
                params![account_id, namespace, list_kind, folder_id],
                |row| {
                    Ok(ChatListCommitAudit {
                        before_count: row.get::<_, i64>(0)? as u64,
                        after_count: row.get::<_, i64>(1)? as u64,
                        is_complete: row.get::<_, i64>(2)? != 0,
                        committed_at_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Every canonical chat in one account namespace, ordered by identity.
    pub fn chats(
        &self,
        scope: gramdrive_model::identity::AccountScope,
    ) -> Result<Vec<ChatRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT chat_id, chat_type, title, username, is_protected, archive_mode,
                    metadata_version, left_at_ms, deleted_at_ms, last_update_at_ms
             FROM chats
             WHERE account_id = ?1 AND namespace_version = ?2
             ORDER BY chat_id",
        )?;
        let rows = statement.query_map(params![account_id, namespace], read_chat)?;
        let mut records = Vec::new();
        for row in rows {
            let raw = row?;
            records.push(ChatRecord {
                key: ChatKey {
                    scope,
                    chat_id: ChatId(raw.chat_id),
                },
                chat_type: ChatType::parse(&raw.chat_type)?,
                title: raw.title,
                username: raw.username,
                is_protected: raw.is_protected,
                archive_mode: raw.archive_mode,
                metadata_version: MetadataVersion::new(raw.metadata_version).map_err(|error| {
                    StateError::CorruptRow {
                        table: "chats",
                        detail: format!("metadata_version does not parse: {error}"),
                    }
                })?,
                left_at_ms: raw.left_at_ms,
                deleted_at_ms: raw.deleted_at_ms,
                last_update_at_ms: raw.last_update_at_ms,
            });
        }
        Ok(records)
    }

    /// Canonical chats that currently have at least one Telegram list
    /// membership, ordered by identity and returned once even when a chat
    /// appears in several lists.
    ///
    /// TDLib's canonical metadata cache is intentionally broader than the
    /// provider namespace. Background history, live initialization, and
    /// progress reconciliation use this set so hidden canonical rows do not
    /// become account-wide work merely because their metadata is durable.
    pub fn listed_chats(
        &self,
        scope: gramdrive_model::identity::AccountScope,
    ) -> Result<Vec<ChatRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT c.chat_id, c.chat_type, c.title, c.username, c.is_protected,
                    c.archive_mode, c.metadata_version, c.left_at_ms,
                    c.deleted_at_ms, c.last_update_at_ms
             FROM chats c
             WHERE c.account_id = ?1 AND c.namespace_version = ?2
               AND EXISTS (
                   SELECT 1 FROM chat_list_entries e
                   WHERE e.account_id = c.account_id
                     AND e.namespace_version = c.namespace_version
                     AND e.chat_id = c.chat_id
               )
             ORDER BY c.chat_id",
        )?;
        let rows = statement.query_map(params![account_id, namespace], read_chat)?;
        let mut records = Vec::new();
        for row in rows {
            let raw = row?;
            records.push(ChatRecord {
                key: ChatKey {
                    scope,
                    chat_id: ChatId(raw.chat_id),
                },
                chat_type: ChatType::parse(&raw.chat_type)?,
                title: raw.title,
                username: raw.username,
                is_protected: raw.is_protected,
                archive_mode: raw.archive_mode,
                metadata_version: MetadataVersion::new(raw.metadata_version).map_err(|error| {
                    StateError::CorruptRow {
                        table: "chats",
                        detail: format!("metadata_version does not parse: {error}"),
                    }
                })?,
                left_at_ms: raw.left_at_ms,
                deleted_at_ms: raw.deleted_at_ms,
                last_update_at_ms: raw.last_update_at_ms,
            });
        }
        Ok(records)
    }

    /// Whether one canonical chat currently belongs to any Telegram list.
    ///
    /// Cursor rows deliberately outlive membership so a later reappearance
    /// resumes without regression; this point read supplies the current
    /// eligibility half of that lifecycle.
    pub fn chat_has_list_membership(&self, key: &ChatKey) -> Result<bool, StateError> {
        let (account_id, namespace) = scope_columns(&key.scope);
        self.conn()
            .prepare_cached(
                "SELECT EXISTS (
                     SELECT 1 FROM chat_list_entries
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                 )",
            )?
            .query_row(params![account_id, namespace, key.chat_id.0], |row| {
                row.get(0)
            })
            .map_err(StateError::from)
    }

    /// One chat's canonical metadata, or `None` if it was never normalized
    /// in this scope.
    pub fn chat(&self, key: &ChatKey) -> Result<Option<ChatRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&key.scope);
        let raw = self
            .conn()
            .prepare_cached(
                "SELECT chat_id, chat_type, title, username, is_protected, archive_mode,
                        metadata_version, left_at_ms, deleted_at_ms, last_update_at_ms
                 FROM chats
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .query_row(params![account_id, namespace, key.chat_id.0], read_chat)
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        Ok(Some(ChatRecord {
            key: ChatKey {
                scope: key.scope,
                chat_id: ChatId(raw.chat_id),
            },
            chat_type: ChatType::parse(&raw.chat_type)?,
            title: raw.title,
            username: raw.username,
            is_protected: raw.is_protected,
            archive_mode: raw.archive_mode,
            metadata_version: MetadataVersion::new(raw.metadata_version).map_err(|error| {
                StateError::CorruptRow {
                    table: "chats",
                    detail: format!("metadata_version does not parse: {error}"),
                }
            })?,
            left_at_ms: raw.left_at_ms,
            deleted_at_ms: raw.deleted_at_ms,
            last_update_at_ms: raw.last_update_at_ms,
        }))
    }

    /// One chat list's membership in its exact presentation order: pinned
    /// first, then Telegram order descending (POL-1).
    pub fn chat_list(&self, list: &ChatListKey) -> Result<Vec<ChatListEntry>, StateError> {
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        let mut statement = self.conn().prepare_cached(
            "SELECT chat_id, pinned, sort_order FROM chat_list_entries
             WHERE account_id = ?1 AND namespace_version = ?2
               AND list_kind = ?3 AND folder_id = ?4
             ORDER BY pinned DESC, sort_order DESC",
        )?;
        let rows = statement.query_map(
            params![account_id, namespace, list_kind, folder_id],
            |row| {
                Ok(ChatListEntry {
                    chat_id: ChatId(row.get(0)?),
                    pinned: row.get(1)?,
                    sort_order: row.get(2)?,
                })
            },
        )?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// One bounded page in Telegram list order, anchored by the last chat id
    /// returned from the previous page. The anchor's persisted position is
    /// resolved inside this read snapshot, so pages never use discovery or
    /// item-id order as a substitute for Telegram order.
    pub fn chat_list_page(
        &self,
        list: &ChatListKey,
        after: Option<ChatId>,
        limit: u32,
    ) -> Result<Vec<ChatListEntry>, StateError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        let anchor = after
            .map(|chat_id| {
                self.conn()
                    .prepare_cached(
                        "SELECT pinned, sort_order FROM chat_list_entries
                         WHERE account_id = ?1 AND namespace_version = ?2
                           AND list_kind = ?3 AND folder_id = ?4 AND chat_id = ?5",
                    )?
                    .query_row(
                        params![account_id, namespace, list_kind, folder_id, chat_id.0],
                        |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?
                    .ok_or(StateError::RowNotFound {
                        entity: "chat list page anchor",
                    })
                    .map(|(pinned, sort_order)| (chat_id, pinned, sort_order))
            })
            .transpose()?;

        let mut entries = Vec::new();
        if let Some((chat_id, pinned, sort_order)) = anchor {
            let mut statement = self.conn().prepare_cached(
                "SELECT chat_id, pinned, sort_order FROM chat_list_entries
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND list_kind = ?3 AND folder_id = ?4
                   AND (pinned < ?5
                     OR (pinned = ?5 AND sort_order < ?6)
                     OR (pinned = ?5 AND sort_order = ?6 AND chat_id < ?7))
                 ORDER BY pinned DESC, sort_order DESC, chat_id DESC LIMIT ?8",
            )?;
            let rows = statement.query_map(
                params![
                    account_id,
                    namespace,
                    list_kind,
                    folder_id,
                    pinned,
                    sort_order,
                    chat_id.0,
                    i64::from(limit),
                ],
                |row| {
                    Ok(ChatListEntry {
                        chat_id: ChatId(row.get(0)?),
                        pinned: row.get(1)?,
                        sort_order: row.get(2)?,
                    })
                },
            )?;
            for row in rows {
                entries.push(row?);
            }
        } else {
            let mut statement = self.conn().prepare_cached(
                "SELECT chat_id, pinned, sort_order FROM chat_list_entries
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND list_kind = ?3 AND folder_id = ?4
                 ORDER BY pinned DESC, sort_order DESC, chat_id DESC LIMIT ?5",
            )?;
            let rows = statement.query_map(
                params![
                    account_id,
                    namespace,
                    list_kind,
                    folder_id,
                    i64::from(limit)
                ],
                |row| {
                    Ok(ChatListEntry {
                        chat_id: ChatId(row.get(0)?),
                        pinned: row.get(1)?,
                        sort_order: row.get(2)?,
                    })
                },
            )?;
            for row in rows {
                entries.push(row?);
            }
        }
        Ok(entries)
    }
}

impl WriteTxn<'_> {
    /// Inserts or fully replaces one chat's canonical metadata.
    ///
    /// Tombstone markers (`left_at_ms`, `deleted_at_ms`) are facts of the
    /// record: POL-3 removes rows only by retention policy, so an
    /// observation that a chat is gone is an upsert with the marker set,
    /// never a delete.
    pub fn upsert_chat(&self, record: &ChatRecord) -> Result<(), StateError> {
        if record.username.as_deref() == Some("") {
            return Err(StateError::InvalidArgument {
                what: "chat username must not be empty text",
            });
        }
        let (account_id, namespace) = scope_columns(&record.key.scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO chats (account_id, namespace_version, chat_id, chat_type, title,
                                    username, is_protected, archive_mode, metadata_version,
                                    left_at_ms, deleted_at_ms, last_update_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (account_id, namespace_version, chat_id) DO UPDATE SET
                     chat_type = excluded.chat_type,
                     title = excluded.title,
                     username = excluded.username,
                     is_protected = excluded.is_protected,
                     archive_mode = excluded.archive_mode,
                     metadata_version = excluded.metadata_version,
                     left_at_ms = excluded.left_at_ms,
                     deleted_at_ms = excluded.deleted_at_ms,
                     last_update_at_ms = excluded.last_update_at_ms",
            )?
            .execute(params![
                account_id,
                namespace,
                record.key.chat_id.0,
                record.chat_type.as_str(),
                record.title,
                record.username,
                record.is_protected,
                record.archive_mode,
                record.metadata_version.as_str(),
                record.left_at_ms,
                record.deleted_at_ms,
                record.last_update_at_ms,
            ])?;
        Ok(())
    }

    /// Replaces one chat list's membership and order with `entries`,
    /// atomically within this transaction (DEC-013 snapshot application).
    ///
    /// Every member chat must already have its canonical row
    /// ([`WriteTxn::upsert_chat`]) — membership references chats, never the
    /// other way around.
    pub fn replace_chat_list(
        &self,
        list: &ChatListKey,
        entries: &[ChatListEntry],
    ) -> Result<(), StateError> {
        self.replace_chat_list_with_audit(list, entries, true, 0)
    }

    /// Replaces a list and atomically records what membership changed.
    ///
    /// `is_complete` is not inferred from row count: the snapshot owner must
    /// explicitly state whether this is a complete source observation. An
    /// omitted existing member is destructive, so it is accepted only when
    /// the source declared this complete *and* the canonical chat carries a
    /// durable departure witness. A rejected shrink leaves this transaction
    /// unchanged; callers retain their prior snapshot checkpoint and retry.
    pub fn replace_chat_list_with_audit(
        &self,
        list: &ChatListKey,
        entries: &[ChatListEntry],
        is_complete: bool,
        committed_at_ms: i64,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        let before_count: i64 = self
            .conn()
            .prepare_cached(
                "SELECT count(*) FROM chat_list_entries
                  WHERE account_id = ?1 AND namespace_version = ?2
                    AND list_kind = ?3 AND folder_id = ?4",
            )?
            .query_row(
                params![account_id, namespace, list_kind, folder_id],
                |row| row.get(0),
            )?;
        let incoming: HashSet<i64> = entries.iter().map(|entry| entry.chat_id.0).collect();
        let mut uncorroborated_removals = 0u64;
        let mut statement = self.conn().prepare_cached(
            "SELECT entry.chat_id, chat.left_at_ms, chat.deleted_at_ms
               FROM chat_list_entries AS entry
               JOIN chats AS chat
                 ON chat.account_id = entry.account_id
                AND chat.namespace_version = entry.namespace_version
                AND chat.chat_id = entry.chat_id
              WHERE entry.account_id = ?1 AND entry.namespace_version = ?2
                AND entry.list_kind = ?3 AND entry.folder_id = ?4",
        )?;
        let rows = statement.query_map(
            params![account_id, namespace, list_kind, folder_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        for row in rows {
            let (chat_id, left_at_ms, deleted_at_ms) = row?;
            if !incoming.contains(&chat_id)
                && (!is_complete || (left_at_ms.is_none() && deleted_at_ms.is_none()))
            {
                uncorroborated_removals = uncorroborated_removals.saturating_add(1);
            }
        }
        if uncorroborated_removals != 0 {
            return Err(StateError::UnsafeChatListShrink {
                before_count: before_count as u64,
                after_count: entries.len() as u64,
                uncorroborated_removals,
            });
        }
        self.conn()
            .prepare_cached(
                "DELETE FROM chat_list_entries
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND list_kind = ?3 AND folder_id = ?4",
            )?
            .execute(params![account_id, namespace, list_kind, folder_id])?;
        let mut insert = self.conn().prepare_cached(
            "INSERT INTO chat_list_entries (account_id, namespace_version, list_kind,
                                            folder_id, chat_id, sort_order, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for entry in entries {
            insert.execute(params![
                account_id,
                namespace,
                list_kind,
                folder_id,
                entry.chat_id.0,
                entry.sort_order,
                entry.pinned,
            ])?;
        }
        self.conn()
            .prepare_cached(
                "INSERT INTO chat_list_commit_audit (
                     account_id, namespace_version, list_kind, folder_id,
                     before_count, after_count, is_complete, committed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                account_id,
                namespace,
                list_kind,
                folder_id,
                before_count,
                entries.len() as i64,
                is_complete,
                committed_at_ms,
            ])?;
        Ok(())
    }

    /// Inserts or updates one chat's membership row in one list, leaving every
    /// other member of the list untouched — the incremental counterpart of
    /// [`WriteTxn::replace_chat_list`] for live `updateChatPosition` deltas
    /// (SYNC-026). Idempotent: re-applying the same `(sort_order, pinned)`
    /// rewrites the row to itself. The member chat must already have its
    /// canonical row ([`WriteTxn::upsert_chat`]); the `chat_list_entries →
    /// chats` foreign key rejects a membership for an unknown chat.
    ///
    /// Order metadata (POL-1's `order.json` and the app's canonical order)
    /// stays consistent because both read `chat_list` sorted by
    /// `pinned DESC, sort_order DESC` — the position lives in the row, never
    /// in insertion order.
    pub fn upsert_chat_list_entry(
        &self,
        list: &ChatListKey,
        entry: &ChatListEntry,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        self.conn()
            .prepare_cached(
                "INSERT INTO chat_list_entries (account_id, namespace_version, list_kind,
                                                folder_id, chat_id, sort_order, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (account_id, namespace_version, list_kind, folder_id, chat_id)
                 DO UPDATE SET sort_order = excluded.sort_order, pinned = excluded.pinned",
            )?
            .execute(params![
                account_id,
                namespace,
                list_kind,
                folder_id,
                entry.chat_id.0,
                entry.sort_order,
                entry.pinned,
            ])?;
        Ok(())
    }

    /// Removes one chat's membership from one list — the chat left the list
    /// (an `updateChatPosition` order 0, or `updateChatRemovedFromList`),
    /// without disturbing the rest of the list or the chat's canonical record
    /// (SYNC-026: leaving a list drops an appearance, never the chat).
    ///
    /// Returns whether a membership row existed: idempotent, so a replayed or
    /// duplicate removal reports `false` rather than failing.
    pub fn remove_chat_list_entry(
        &self,
        list: &ChatListKey,
        chat_id: ChatId,
    ) -> Result<bool, StateError> {
        let (account_id, namespace) = scope_columns(&list.scope);
        let (list_kind, folder_id) = list_columns(list.kind)?;
        let affected = self
            .conn()
            .prepare_cached(
                "DELETE FROM chat_list_entries
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND list_kind = ?3 AND folder_id = ?4 AND chat_id = ?5",
            )?
            .execute(params![
                account_id, namespace, list_kind, folder_id, chat_id.0,
            ])?;
        Ok(affected > 0)
    }
}
