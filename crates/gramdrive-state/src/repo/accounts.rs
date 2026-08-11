//! Account rows: per-account policy facts and the namespace epoch
//! (DOM-021, POL-2, POL-3).

use std::collections::BTreeSet;

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ItemId, ItemKey, NamespaceVersion,
    StoryAppearanceLocation,
};
use gramdrive_model::version::MetadataVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, namespace_from_column};

/// Which source implementation serves an account (`accounts.source_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A local TDLib session.
    LocalTdlib,
    /// A remote HTTP drive service.
    RemoteHttp,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LocalTdlib => "local_tdlib",
            Self::RemoteHttp => "remote_http",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "local_tdlib" => Ok(Self::LocalTdlib),
            "remote_http" => Ok(Self::RemoteHttp),
            other => Err(StateError::CorruptRow {
                table: "accounts",
                detail: format!("unknown source_kind '{other}'"),
            }),
        }
    }
}

/// The per-account POL-3 retention selection (`accounts.retention_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// Mirror mode: content purged on observed deletion, minimal markers
    /// kept for sync correctness.
    Mirror,
    /// Audit mode: observed history retained until explicitly purged.
    Audit,
}

/// Capability proving that the caller typed the account-specific destructive
/// confirmation for an Audit-to-Mirror transition.
///
/// The fields are private so callers cannot manufacture approval with a bool.
/// Obtain one through [`AuditToMirrorConfirmation::parse`], then pass it to
/// [`WriteTxn::set_retention_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditToMirrorConfirmation {
    account: AccountKey,
}

impl AuditToMirrorConfirmation {
    /// The exact phrase the user must type for `account`.
    pub fn expected_phrase(account: AccountKey) -> String {
        format!("PURGE ACCOUNT {} AUDIT HISTORY", account.account_id.0)
    }

    /// Validates typed confirmation and returns a capability scoped to exactly
    /// one account. Whitespace and case are intentional parts of the phrase.
    pub fn parse(account: AccountKey, typed: &str) -> Result<Self, StateError> {
        if typed != Self::expected_phrase(account) {
            return Err(StateError::InvalidArgument {
                what: "Audit-to-Mirror confirmation phrase does not match the account",
            });
        }
        Ok(Self { account })
    }

    fn authorizes(self, account: AccountKey) -> bool {
        self.account == account
    }
}

impl RetentionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Audit => "audit",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "mirror" => Ok(Self::Mirror),
            "audit" => Ok(Self::Audit),
            other => Err(StateError::CorruptRow {
                table: "accounts",
                detail: format!("unknown retention_mode '{other}'"),
            }),
        }
    }
}

/// What a [`WriteTxn::set_retention_mode`] call did (POL-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionChange {
    /// The mode before the call.
    pub previous: RetentionMode,
    /// The mode after the call (equal to `previous` when nothing changed).
    pub current: RetentionMode,
    /// Event rows whose content this call purged — non-zero only when
    /// switching to Mirror retroactively purges retained history.
    pub purged_events: usize,
    /// Deleted-message attachment metadata rows removed from Audit storage.
    pub purged_attachments: usize,
    /// Superseded attachment-version metadata rows removed from Audit storage.
    pub purged_attachment_versions: usize,
    /// Audit-retained removed profile-story rows removed.
    pub purged_stories: usize,
    /// Verified blob records left with no attachment, story, or cache owner.
    pub purged_blobs: usize,
    /// Materialized cache rows whose content no longer has a Mirror owner.
    pub purged_cache_entries: usize,
    /// Offline pins released with those removed items.
    pub released_pins: usize,
    /// Cache objects durably queued for idempotent filesystem deletion.
    pub queued_file_purges: usize,
    /// Provider item rows tombstoned so open enumerations invalidate them.
    pub invalidated_items: usize,
    /// Generated documents marked dirty for re-render — every one of the
    /// account's, because the retention mode is stamped in each document's
    /// header, so any switch changes their bytes.
    pub invalidated_docs: usize,
}

/// What changing the independent Archive-Mode byte policy did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveModeChange {
    /// Stored value before the call.
    pub previous: bool,
    /// Stored value after the call.
    pub current: bool,
    /// Newly created Archive-Mode pins for allowed persistent content.
    pub pinned_items: usize,
    /// Archive-Mode pins released. Explicit user pins are never included.
    pub released_items: usize,
}

impl ArchiveModeChange {
    /// Whether the independent toggle actually moved.
    pub fn changed(self) -> bool {
        self.previous != self.current
    }
}

impl RetentionChange {
    /// Whether the mode actually moved. `false` means the requested mode was
    /// already in effect and nothing was purged or invalidated.
    pub fn changed(self) -> bool {
        self.previous != self.current
    }
}

/// What a [`WriteTxn::set_display_timezone`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTimezoneChange {
    /// Persisted timezone before the call.
    pub previous: String,
    /// Persisted timezone after the call.
    pub current: String,
    /// Generated documents marked dirty because their civil partition or
    /// rendered provenance can change.
    pub invalidated_docs: usize,
}

impl DisplayTimezoneChange {
    /// Whether the persisted policy actually changed.
    pub fn changed(&self) -> bool {
        self.previous != self.current
    }
}

/// One configured account (domain-model § Account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    /// The account's stable identity.
    pub account: AccountKey,
    /// Which source implementation serves it.
    pub source_kind: SourceKind,
    /// Display name shown as the account's root directory.
    pub display_name: String,
    /// Source-defined authorization state text (never secret material).
    pub auth_state: String,
    /// Current identity-namespace epoch (DOM-021). Advanced only by
    /// [`WriteTxn::bump_namespace`]; ignored on upsert of an existing row.
    pub namespace_version: NamespaceVersion,
    /// IANA display timezone used only for filenames and civil partitions.
    /// Source timestamps remain absolute UTC milliseconds.
    pub display_timezone: String,
    /// POL-3 retention selection.
    pub retention_mode: RetentionMode,
    /// POL-2 Archive-Mode toggle.
    pub archive_mode: bool,
    /// Reference into platform secure storage — never key material.
    pub secret_ref: Option<String>,
    /// When the account was configured (ms since the Unix epoch).
    pub created_at_ms: i64,
    /// Last update to this row (ms since the Unix epoch).
    pub updated_at_ms: i64,
}

impl AccountRecord {
    /// The account's current scope: identity plus namespace epoch.
    pub fn scope(&self) -> AccountScope {
        AccountScope {
            account: self.account,
            namespace_version: self.namespace_version,
        }
    }
}

/// One account row exactly as stored, before enum text and namespace
/// columns are converted (conversion can fail, and `rusqlite` row mapping
/// cannot carry [`StateError`]).
struct RawAccount {
    account_id: i64,
    source_kind: String,
    display_name: String,
    auth_state: String,
    namespace_version: i64,
    display_timezone: String,
    retention_mode: String,
    archive_mode: bool,
    secret_ref: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

fn read_account(row: &Row<'_>) -> Result<RawAccount, rusqlite::Error> {
    Ok(RawAccount {
        account_id: row.get("account_id")?,
        source_kind: row.get("source_kind")?,
        display_name: row.get("display_name")?,
        auth_state: row.get("auth_state")?,
        namespace_version: row.get("namespace_version")?,
        display_timezone: row.get("display_timezone")?,
        retention_mode: row.get("retention_mode")?,
        archive_mode: row.get("archive_mode")?,
        secret_ref: row.get("secret_ref")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
    })
}

fn finish_account(raw: RawAccount) -> Result<AccountRecord, StateError> {
    Ok(AccountRecord {
        account: AccountKey {
            account_id: AccountId(raw.account_id),
        },
        source_kind: SourceKind::parse(&raw.source_kind)?,
        display_name: raw.display_name,
        auth_state: raw.auth_state,
        namespace_version: namespace_from_column("accounts", raw.namespace_version)?,
        display_timezone: raw.display_timezone,
        retention_mode: RetentionMode::parse(&raw.retention_mode)?,
        archive_mode: raw.archive_mode,
        secret_ref: raw.secret_ref,
        created_at_ms: raw.created_at_ms,
        updated_at_ms: raw.updated_at_ms,
    })
}

const SELECT_ACCOUNT: &str = "SELECT account_id, source_kind, display_name, auth_state,
            namespace_version, display_timezone, retention_mode, archive_mode, secret_ref,
            created_at_ms, updated_at_ms
     FROM accounts";

impl ReadTxn<'_> {
    /// One account by identity, or `None` if it is not configured.
    pub fn account(&self, account: AccountKey) -> Result<Option<AccountRecord>, StateError> {
        let parts = self
            .conn()
            .prepare_cached(&format!("{SELECT_ACCOUNT} WHERE account_id = ?1"))?
            .query_row(params![account.account_id.0], read_account)
            .optional()?;
        parts.map(finish_account).transpose()
    }

    /// Every configured account, in identity order.
    pub fn accounts(&self) -> Result<Vec<AccountRecord>, StateError> {
        let mut statement = self
            .conn()
            .prepare_cached(&format!("{SELECT_ACCOUNT} ORDER BY account_id"))?;
        let rows = statement.query_map([], read_account)?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(finish_account(row?)?);
        }
        Ok(accounts)
    }

    /// The account's POL-3 retention mode, or `None` if the account is not
    /// configured. The lean read the change appliers do once per batch, so
    /// the stored column — not a caller-supplied value — is always what
    /// governs content purging.
    pub fn retention_mode(&self, account: AccountKey) -> Result<Option<RetentionMode>, StateError> {
        let text: Option<String> = self
            .conn()
            .prepare_cached("SELECT retention_mode FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()?;
        text.map(|text| RetentionMode::parse(&text)).transpose()
    }

    /// The persisted IANA display timezone, or `None` when the account is not
    /// configured.
    pub fn display_timezone(&self, account: AccountKey) -> Result<Option<String>, StateError> {
        self.conn()
            .prepare_cached("SELECT display_timezone FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()
            .map_err(StateError::from)
    }

    /// Monotonic generation for byte-shaping account render policy.
    pub fn render_generation(&self, account: AccountKey) -> Result<Option<i64>, StateError> {
        self.conn()
            .prepare_cached("SELECT render_generation FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()
            .map_err(StateError::from)
    }

    /// The current scope of an account — its identity at today's namespace
    /// epoch — or `None` if the account is not configured.
    pub fn current_scope(&self, account: AccountKey) -> Result<Option<AccountScope>, StateError> {
        let namespace: Option<i64> = self
            .conn()
            .prepare_cached("SELECT namespace_version FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()?;
        namespace
            .map(|value| {
                Ok(AccountScope {
                    account,
                    namespace_version: namespace_from_column("accounts", value)?,
                })
            })
            .transpose()
    }
}

impl WriteTxn<'_> {
    /// Inserts the account, or updates every fact of an existing row except
    /// the namespace epoch, creation time, retention mode, and display
    /// timezone.
    ///
    /// The epoch is excluded on purpose: it only moves forward, through
    /// [`WriteTxn::bump_namespace`], so a stale in-memory record replayed
    /// through upsert can never rewind it (DOM-021).
    ///
    /// Retention mode and display timezone are excluded from the *update*
    /// because changing either mid-life is not a plain metadata write. A
    /// retention transition may purge history; a timezone transition can move
    /// messages between direct month partitions. Both change generated bytes.
    /// Existing accounts therefore use [`WriteTxn::set_retention_mode`] and
    /// [`WriteTxn::set_display_timezone`] inside coordinator-owned atomic
    /// policy transitions. On *insert* both values are honored (account setup).
    pub fn upsert_account(&self, record: &AccountRecord) -> Result<(), StateError> {
        if record.auth_state.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "account auth_state must not be empty",
            });
        }
        if record.secret_ref.as_deref() == Some("") {
            return Err(StateError::InvalidArgument {
                what: "account secret_ref must not be empty text",
            });
        }
        if record.display_timezone.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "account display_timezone must not be empty",
            });
        }
        self.conn()
            .prepare_cached(
                "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                                       namespace_version, display_timezone, retention_mode,
                                       archive_mode, secret_ref, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (account_id) DO UPDATE SET
                     source_kind = excluded.source_kind,
                     display_name = excluded.display_name,
                     auth_state = excluded.auth_state,
                     secret_ref = excluded.secret_ref,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                record.account.account_id.0,
                record.source_kind.as_str(),
                record.display_name,
                record.auth_state,
                i64::from(record.namespace_version.0),
                record.display_timezone,
                record.retention_mode.as_str(),
                record.archive_mode,
                record.secret_ref,
                record.created_at_ms,
                record.updated_at_ms,
            ])?;
        Ok(())
    }

    /// Changes the persisted display timezone and invalidates all generated
    /// documents in the same transaction.
    ///
    /// The namespace coordinator must call this together with its tree
    /// reconciliation before committing: changing civil time can replace the
    /// direct `YYYY-MM` partition set, while months that remain still change
    /// their Markdown timezone provenance. Keeping this primitive on the write
    /// transaction lets the account write, old-partition tombstones,
    /// new-partition inserts, and dirty worklist become one atomic commit.
    ///
    /// The state layer validates only non-empty storage text; the coordinator
    /// resolves the value as an IANA timezone before opening the transaction.
    pub fn set_display_timezone(
        &self,
        account: AccountKey,
        timezone: &str,
        updated_at_ms: i64,
    ) -> Result<DisplayTimezoneChange, StateError> {
        if timezone.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "account display_timezone must not be empty",
            });
        }
        let previous = self
            .read()
            .display_timezone(account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        if previous == timezone {
            return Ok(DisplayTimezoneChange {
                previous,
                current: timezone.to_owned(),
                invalidated_docs: 0,
            });
        }
        self.conn()
            .prepare_cached(
                "UPDATE accounts
                 SET display_timezone = ?2,
                     render_generation = render_generation + 1,
                     updated_at_ms = ?3
                 WHERE account_id = ?1",
            )?
            .execute(params![account.account_id.0, timezone, updated_at_ms])?;
        let invalidated_docs = self
            .conn()
            .prepare_cached(
                "UPDATE render_state SET dirty = 1
                 WHERE item_id IN (
                     SELECT item_id FROM items
                     WHERE account_id = ?1 AND kind = 'generated_doc'
                 )",
            )?
            .execute(params![account.account_id.0])?;
        Ok(DisplayTimezoneChange {
            previous,
            current: timezone.to_owned(),
            invalidated_docs,
        })
    }

    /// Advances the account's namespace epoch by one and returns the new
    /// epoch (DOM-021).
    ///
    /// Rows of the retired epoch stay until reconciliation sweeps them; this
    /// operation only moves the boundary.
    pub fn bump_namespace(
        &self,
        account: AccountKey,
        updated_at_ms: i64,
    ) -> Result<NamespaceVersion, StateError> {
        let namespace: Option<i64> = self
            .conn()
            .prepare_cached(
                "UPDATE accounts
                 SET namespace_version = namespace_version + 1, updated_at_ms = ?2
                 WHERE account_id = ?1
                 RETURNING namespace_version",
            )?
            .query_row(params![account.account_id.0, updated_at_ms], |row| {
                row.get(0)
            })
            .optional()?;
        match namespace {
            Some(value) => namespace_from_column("accounts", value),
            None => Err(StateError::RowNotFound { entity: "account" }),
        }
    }

    /// Removes an account and every row scoped to it — the `PurgeState`
    /// step of the SEC-004 removal sequence.
    ///
    /// One `DELETE` on `accounts`: every account-scoped table reaches
    /// `accounts` through `ON DELETE CASCADE` (directly, or via `chats` /
    /// `items` / `messages`), so the schema — not a hand-maintained table
    /// list — guarantees nothing account-scoped survives. Returns whether
    /// the account existed; purging an absent account is a no-op success,
    /// so an interrupted removal re-runs into a completed one.
    pub fn purge_account(&self, account_id: AccountId) -> Result<bool, StateError> {
        let removed = self
            .conn()
            .prepare_cached("DELETE FROM accounts WHERE account_id = ?1")?
            .execute(params![account_id.0])?;
        Ok(removed > 0)
    }

    /// Changes an account's POL-3 retention mode, applying the consequences
    /// atomically (DEC-015).
    ///
    /// A no-op when the mode is already in effect. Otherwise, in one
    /// transaction with the column write:
    ///
    /// * **Switching to Mirror** purges the history Audit retained — every
    ///   superseded revision and every deleted message's content across the
    ///   account, keeping only the current revision of each live message.
    ///   Content that was already purged, or history never observed, is not
    ///   invented back; there is no recovery, only forgetting (POL-3 scope).
    /// * **Switching to Audit** purges nothing and recovers nothing:
    ///   already-purged content is gone for good. Audit history begins
    ///   accumulating from this point forward.
    ///
    /// Both directions mark every one of the account's generated documents
    /// dirty, because the retention mode is written into each document's
    /// header — a switch changes their bytes even where the message set is
    /// unchanged. The re-render happens through the normal watermark protocol
    /// (SYNC-024); this call only records that the work is due.
    ///
    /// Returns [`RetentionChange`] describing what happened; the account must
    /// exist ([`StateError::RowNotFound`]).
    pub fn set_retention_mode(
        &self,
        account: AccountKey,
        mode: RetentionMode,
        confirmation: Option<AuditToMirrorConfirmation>,
        updated_at_ms: i64,
    ) -> Result<RetentionChange, StateError> {
        let previous = self
            .read()
            .retention_mode(account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        if previous == mode {
            return Ok(RetentionChange {
                previous,
                current: mode,
                purged_events: 0,
                purged_attachments: 0,
                purged_attachment_versions: 0,
                purged_stories: 0,
                purged_blobs: 0,
                purged_cache_entries: 0,
                released_pins: 0,
                queued_file_purges: 0,
                invalidated_items: 0,
                invalidated_docs: 0,
            });
        }

        if previous == RetentionMode::Audit
            && mode == RetentionMode::Mirror
            && !confirmation.is_some_and(|approval| approval.authorizes(account))
        {
            return Err(StateError::InvalidArgument {
                what: "Audit-to-Mirror requires typed account-scoped confirmation",
            });
        }

        let purge_items = if mode == RetentionMode::Mirror {
            self.audit_only_item_ids(account)?
        } else {
            Vec::new()
        };

        self.conn()
            .prepare_cached(
                "UPDATE accounts
                 SET retention_mode = ?2,
                     render_generation = render_generation + 1,
                     updated_at_ms = ?3
                 WHERE account_id = ?1",
            )?
            .execute(params![account.account_id.0, mode.as_str(), updated_at_ms])?;

        // Switching to Mirror applies its invariant retroactively: purge every
        // event payload that is not the current revision of a live message.
        // A deleted message's rows are all superseded (is_deleted = 1 excludes
        // it from the keep set), so its content goes; a live message keeps
        // exactly the one revision the projection points at.
        let purged_events = if mode == RetentionMode::Mirror {
            self.conn()
                .prepare_cached(
                    "UPDATE message_events SET payload = NULL, payload_schema = NULL
                     WHERE account_id = ?1 AND payload IS NOT NULL
                       AND event_seq NOT IN (
                           SELECT latest_event_seq FROM messages
                           WHERE account_id = ?1 AND is_deleted = 0
                       )",
                )?
                .execute(params![account.account_id.0])?
        } else {
            0
        };

        let mut purged_cache_entries = 0;
        let mut released_pins = 0;
        let mut queued_file_purges = 0;
        let mut invalidated_items = 0;
        if mode == RetentionMode::Mirror {
            let metadata = MetadataVersion::new(format!("retention-purge-{updated_at_ms}"))
                .map_err(|_| StateError::InvalidArgument {
                    what: "retention purge timestamp cannot form a metadata version",
                })?;
            for item in &purge_items {
                let reference: Option<String> = self
                    .conn()
                    .prepare_cached(
                        "SELECT materialization_ref FROM cache_entries WHERE item_id = ?1",
                    )?
                    .query_row(params![item.as_bytes()], |row| row.get(0))
                    .optional()?
                    .flatten();
                released_pins += self
                    .conn()
                    .prepare_cached("DELETE FROM pins WHERE item_id = ?1")?
                    .execute(params![item.as_bytes()])?;
                purged_cache_entries += self
                    .conn()
                    .prepare_cached("DELETE FROM cache_entries WHERE item_id = ?1")?
                    .execute(params![item.as_bytes()])?;
                if self
                    .read()
                    .item(item)?
                    .is_some_and(|stored| stored.deleted_at_ms.is_none())
                {
                    self.tombstone_item_with_provenance(
                        item,
                        updated_at_ms,
                        &metadata,
                        super::TombstoneProvenance::Retention,
                    )?;
                    invalidated_items += 1;
                }
                if let Some(reference) = reference {
                    queued_file_purges += self
                        .conn()
                        .prepare_cached(
                            "INSERT INTO retention_purge_queue (
                                 account_id, materialization_ref, queued_at_ms)
                             VALUES (?1, ?2, ?3)
                             ON CONFLICT (account_id, materialization_ref) DO NOTHING",
                        )?
                        .execute(params![account.account_id.0, reference, updated_at_ms])?;
                }
            }
        }

        let purged_attachments = if mode == RetentionMode::Mirror {
            self.conn()
                .prepare_cached(
                    "DELETE FROM attachments
                     WHERE account_id = ?1 AND EXISTS (
                         SELECT 1 FROM messages m
                         WHERE m.account_id = attachments.account_id
                           AND m.namespace_version = attachments.namespace_version
                           AND m.chat_id = attachments.chat_id
                           AND m.message_id = attachments.message_id
                           AND m.is_deleted = 1
                     )",
                )?
                .execute(params![account.account_id.0])?
        } else {
            0
        };

        let purged_attachment_versions = if mode == RetentionMode::Mirror {
            let references = {
                let mut statement = self.conn().prepare_cached(
                    "SELECT materialization_ref
                     FROM retained_attachment_versions
                     WHERE account_id = ?1 AND materialization_ref IS NOT NULL",
                )?;
                let rows = statement
                    .query_map(params![account.account_id.0], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for reference in references {
                queued_file_purges += self
                    .conn()
                    .prepare_cached(
                        "INSERT INTO retention_purge_queue (
                             account_id, materialization_ref, queued_at_ms)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT (account_id, materialization_ref) DO NOTHING",
                    )?
                    .execute(params![account.account_id.0, reference, updated_at_ms])?;
            }
            self.conn()
                .prepare_cached("DELETE FROM retained_attachment_versions WHERE account_id = ?1")?
                .execute(params![account.account_id.0])?
        } else {
            0
        };

        let purged_stories = if mode == RetentionMode::Mirror {
            let appearances = self
                .conn()
                .prepare_cached(
                    "DELETE FROM story_appearances
                     WHERE account_id = ?1 AND removed_at_ms IS NOT NULL",
                )?
                .execute(params![account.account_id.0])?;
            let stories = self
                .conn()
                .prepare_cached(
                    "DELETE FROM stories
                     WHERE account_id = ?1 AND NOT EXISTS (
                           SELECT 1 FROM story_appearances a
                           WHERE a.account_id = stories.account_id
                             AND a.namespace_version = stories.namespace_version
                             AND a.poster_chat_id = stories.poster_chat_id
                             AND a.story_id = stories.story_id
                       )",
                )?
                .execute(params![account.account_id.0])?;
            appearances.saturating_add(stories)
        } else {
            0
        };

        let purged_blobs = if mode == RetentionMode::Mirror {
            self.conn()
                .prepare_cached(
                    "DELETE FROM blobs
                     WHERE account_id = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM attachments a
                           WHERE a.account_id = blobs.account_id
                             AND a.blob_hash_algo = blobs.hash_algo AND a.blob_hash = blobs.hash
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM stories s
                           WHERE s.account_id = blobs.account_id
                             AND s.blob_hash_algo = blobs.hash_algo AND s.blob_hash = blobs.hash
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM cache_entries c
                           WHERE c.account_id = blobs.account_id
                             AND c.blob_hash_algo = blobs.hash_algo AND c.blob_hash = blobs.hash
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM retained_attachment_versions r
                           WHERE r.account_id = blobs.account_id
                             AND r.blob_hash_algo = blobs.hash_algo AND r.blob_hash = blobs.hash
                       )",
                )?
                .execute(params![account.account_id.0])?
        } else {
            0
        };

        // Every generated document carries the mode in its header, so all of
        // the account's re-render (SYNC-024). Docs never rendered have no
        // render_state row and are already stale by absence.
        let invalidated_docs = self
            .conn()
            .prepare_cached(
                "UPDATE render_state SET dirty = 1
                 WHERE item_id IN (
                     SELECT item_id FROM items
                     WHERE account_id = ?1 AND kind = 'generated_doc'
                 )",
            )?
            .execute(params![account.account_id.0])?;

        Ok(RetentionChange {
            previous,
            current: mode,
            purged_events,
            purged_attachments,
            purged_attachment_versions,
            purged_stories,
            purged_blobs,
            purged_cache_entries,
            released_pins,
            queued_file_purges,
            invalidated_items,
            invalidated_docs,
        })
    }

    /// Changes Archive Mode without touching the retention selection.
    /// Allowed live attachment and persistent-story items receive durable
    /// Archive-Mode pins; restricted/unavailable/tombstoned items never do.
    pub fn set_archive_mode(
        &self,
        account: AccountKey,
        enabled: bool,
        updated_at_ms: i64,
    ) -> Result<ArchiveModeChange, StateError> {
        let previous: bool = self
            .conn()
            .prepare_cached("SELECT archive_mode FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        if previous == enabled {
            return Ok(ArchiveModeChange {
                previous,
                current: enabled,
                pinned_items: 0,
                released_items: 0,
            });
        }

        self.conn()
            .prepare_cached(
                "UPDATE accounts SET archive_mode = ?2, updated_at_ms = ?3
                 WHERE account_id = ?1",
            )?
            .execute(params![account.account_id.0, enabled, updated_at_ms])?;

        let (pinned_items, released_items) = if enabled {
            let candidates = self.archive_candidate_ids(account)?;
            let mut pinned = 0;
            for item in candidates {
                pinned += self
                    .conn()
                    .prepare_cached(
                        "INSERT INTO pins (item_id, origin, created_at_ms)
                         VALUES (?1, 'archive_mode', ?2)
                         ON CONFLICT (item_id) DO NOTHING",
                    )?
                    .execute(params![item.as_bytes(), updated_at_ms])?;
                self.conn()
                    .prepare_cached(
                        "UPDATE cache_entries
                         SET pinned = 1,
                             pin_origin = CASE
                                 WHEN pin_origin = 'user' THEN 'user' ELSE 'archive_mode' END
                         WHERE item_id = ?1",
                    )?
                    .execute(params![item.as_bytes()])?;
            }
            (pinned, 0)
        } else {
            let mut statement = self.conn().prepare_cached(
                "SELECT p.item_id FROM pins p JOIN items i ON i.item_id = p.item_id
                 WHERE i.account_id = ?1 AND p.origin = 'archive_mode'",
            )?;
            let rows = statement.query_map(params![account.account_id.0], |row| {
                row.get::<_, Vec<u8>>(0)
            })?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(crate::repo::item_id_from_column("pins", &row?)?);
            }
            drop(statement);
            for item in &ids {
                self.conn()
                    .prepare_cached(
                        "DELETE FROM pins WHERE item_id = ?1 AND origin = 'archive_mode'",
                    )?
                    .execute(params![item.as_bytes()])?;
                self.conn()
                    .prepare_cached(
                        "UPDATE cache_entries SET pinned = 0, pin_origin = NULL
                         WHERE item_id = ?1 AND pin_origin = 'archive_mode'",
                    )?
                    .execute(params![item.as_bytes()])?;
            }
            (0, ids.len())
        };

        Ok(ArchiveModeChange {
            previous,
            current: enabled,
            pinned_items,
            released_items,
        })
    }

    fn archive_candidate_ids(&self, account: AccountKey) -> Result<Vec<ItemId>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id FROM items
             WHERE account_id = ?1 AND deleted_at_ms IS NULL
               AND availability = 'fetchable' AND content_version IS NOT NULL
               AND kind IN ('attachment', 'story_appearance')
             ORDER BY item_id",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut items = Vec::new();
        for row in rows {
            let item = crate::repo::item_id_from_column("items", &row?)?;
            if matches!(
                item.key(),
                ItemKey::StoryAppearance(ref appearance)
                    if appearance.location == StoryAppearanceLocation::Active
            ) {
                continue;
            }
            items.push(item);
        }
        Ok(items)
    }

    fn audit_only_item_ids(&self, account: AccountKey) -> Result<Vec<ItemId>, StateError> {
        let mut deleted_messages = BTreeSet::new();
        {
            let mut statement = self.conn().prepare_cached(
                "SELECT namespace_version, chat_id, message_id FROM messages
                 WHERE account_id = ?1 AND is_deleted = 1",
            )?;
            let rows = statement.query_map(params![account.account_id.0], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                deleted_messages.insert(row?);
            }
        }
        let mut removed_stories = BTreeSet::new();
        {
            let mut statement = self.conn().prepare_cached(
                "SELECT namespace_version, poster_chat_id, story_id
                 FROM story_appearances
                 WHERE account_id = ?1 AND removed_at_ms IS NOT NULL",
            )?;
            let rows = statement.query_map(params![account.account_id.0], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                removed_stories.insert(row?);
            }
        }

        let mut statement = self.conn().prepare_cached(
            "SELECT item_id, deleted_at_ms FROM items
             WHERE account_id = ?1
               AND kind IN ('attachment', 'canonical_story', 'story_appearance')",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut purge = Vec::new();
        for row in rows {
            let (bytes, deleted_at_ms) = row?;
            let item = crate::repo::item_id_from_column("items", &bytes)?;
            let purge_item = deleted_at_ms.is_some()
                || match item.key() {
                    ItemKey::Canonical(CanonicalKey::Attachment(key))
                    | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
                        item: CanonicalKey::Attachment(key),
                        ..
                    }) => deleted_messages.contains(&(
                        i64::from(key.message.chat.scope.namespace_version.0),
                        key.message.chat.chat_id.0,
                        key.message.message_id.0,
                    )),
                    ItemKey::Canonical(CanonicalKey::Story(key))
                    | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
                        item: CanonicalKey::Story(key),
                        ..
                    }) => removed_stories.contains(&(
                        i64::from(key.poster.scope.namespace_version.0),
                        key.poster.chat_id.0,
                        key.story_id.0,
                    )),
                    ItemKey::StoryAppearance(appearance) => removed_stories.contains(&(
                        i64::from(appearance.story.poster.scope.namespace_version.0),
                        appearance.story.poster.chat_id.0,
                        appearance.story.story_id.0,
                    )),
                    _ => false,
                };
            if purge_item {
                purge.push(item);
            }
        }
        Ok(purge)
    }
}
