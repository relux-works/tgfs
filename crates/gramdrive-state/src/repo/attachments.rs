//! Attachment facts and verified blobs (DOM-007, SYNC-045, SYNC-052).
//!
//! Attachment identity is (chat, message, ordinal); Telegram locators are
//! refreshable metadata, never identity. That split is structural here:
//! [`WriteTxn::upsert_attachment`] rewrites the metadata columns and leaves
//! the blob link alone, so a locator refresh (SYNC-045) can never detach
//! verified bytes, and [`WriteTxn::link_attachment_blob`] is the only way
//! bytes attach — after verification, in the transaction that verified them.

use gramdrive_model::identity::{
    AccountKey, AccountScope, AttachmentIndex, AttachmentKey, ChatId, ChatKey, ContentHash,
    MessageId, MessageKey,
};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    ReadTxn, WriteTxn, hash_columns, hash_from_columns, namespace_from_column, scope_columns,
    size_from_column, size_to_column,
};

/// Content availability of an attachment (`attachments.availability`,
/// POL-4). Unlike a provider item, an attachment can be view-once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentAvailability {
    /// Bytes can be fetched.
    Fetchable,
    /// Telegram restricts the content; bytes are never fetched (POL-4).
    Restricted,
    /// The content is gone at the source.
    Unavailable,
    /// View-once content; bytes are never fetched (POL-4).
    ViewOnce,
}

impl AttachmentAvailability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fetchable => "fetchable",
            Self::Restricted => "restricted",
            Self::Unavailable => "unavailable",
            Self::ViewOnce => "view_once",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "fetchable" => Ok(Self::Fetchable),
            "restricted" => Ok(Self::Restricted),
            "unavailable" => Ok(Self::Unavailable),
            "view_once" => Ok(Self::ViewOnce),
            other => Err(StateError::CorruptRow {
                table: "attachments",
                detail: format!("unknown availability '{other}'"),
            }),
        }
    }
}

/// The refreshable facts of one attachment (domain-model § Attachment).
///
/// Everything here may change on a locator refresh; nothing here is
/// identity. The blob link is deliberately absent — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFacts {
    /// The attachment's identity.
    pub key: AttachmentKey,
    /// Original file name, if the message carried one.
    pub original_name: Option<String>,
    /// MIME type, if known.
    pub mime_type: Option<String>,
    /// Logical size in bytes, if known.
    pub logical_size: Option<u64>,
    /// Version the bytes are fetched under (DOM-003, SYNC-042).
    pub content_version: ContentVersion,
    /// Telegram's stable file identifier, if known.
    pub telegram_unique_id: Option<String>,
    /// Telegram's refreshable file locator (SYNC-045).
    pub telegram_file_id: Option<String>,
    /// Telegram's refreshable access reference (SYNC-045).
    pub file_reference: Option<Vec<u8>>,
    /// POL-4 availability.
    pub availability: AttachmentAvailability,
    /// Telegram's can-be-saved flag (POL-4).
    pub can_be_saved: bool,
}

/// One attachment as stored: refreshable facts plus the verified blob link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentState {
    /// The refreshable facts.
    pub facts: AttachmentFacts,
    /// Hash of the verified bytes, once a download completed (SYNC-042).
    pub blob_hash: Option<ContentHash>,
    /// When the blob link was last verified (ms since the Unix epoch).
    pub last_verified_at_ms: Option<i64>,
}

/// One fully downloaded, hash-verified blob (domain-model § Blob).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRecord {
    /// Content hash of the complete bytes.
    pub hash: ContentHash,
    /// Size in bytes.
    pub size: u64,
    /// When the blob first became known (ms since the Unix epoch).
    pub first_seen_at_ms: i64,
}

struct RawAttachment {
    attachment_index: i64,
    original_name: Option<String>,
    mime_type: Option<String>,
    logical_size: Option<i64>,
    content_version: String,
    telegram_unique_id: Option<String>,
    telegram_file_id: Option<String>,
    file_reference: Option<Vec<u8>>,
    availability: String,
    can_be_saved: bool,
    blob_hash_algo: Option<String>,
    blob_hash: Option<Vec<u8>>,
    last_verified_at_ms: Option<i64>,
}

fn read_attachment(row: &Row<'_>) -> Result<RawAttachment, rusqlite::Error> {
    Ok(RawAttachment {
        attachment_index: row.get("attachment_index")?,
        original_name: row.get("original_name")?,
        mime_type: row.get("mime_type")?,
        logical_size: row.get("logical_size")?,
        content_version: row.get("content_version")?,
        telegram_unique_id: row.get("telegram_unique_id")?,
        telegram_file_id: row.get("telegram_file_id")?,
        file_reference: row.get("file_reference")?,
        availability: row.get("availability")?,
        can_be_saved: row.get("can_be_saved")?,
        blob_hash_algo: row.get("blob_hash_algo")?,
        blob_hash: row.get("blob_hash")?,
        last_verified_at_ms: row.get("last_verified_at_ms")?,
    })
}

fn finish_attachment(
    message: MessageKey,
    raw: RawAttachment,
) -> Result<AttachmentState, StateError> {
    let index = u32::try_from(raw.attachment_index).map_err(|_| StateError::CorruptRow {
        table: "attachments",
        detail: format!("attachment_index {} does not fit u32", raw.attachment_index),
    })?;
    Ok(AttachmentState {
        facts: AttachmentFacts {
            key: AttachmentKey {
                message,
                index: AttachmentIndex(index),
            },
            original_name: raw.original_name,
            mime_type: raw.mime_type,
            logical_size: raw
                .logical_size
                .map(|size| size_from_column("attachments", size))
                .transpose()?,
            content_version: ContentVersion::new(raw.content_version).map_err(|error| {
                StateError::CorruptRow {
                    table: "attachments",
                    detail: format!("content_version does not parse: {error}"),
                }
            })?,
            telegram_unique_id: raw.telegram_unique_id,
            telegram_file_id: raw.telegram_file_id,
            file_reference: raw.file_reference,
            availability: AttachmentAvailability::parse(&raw.availability)?,
            can_be_saved: raw.can_be_saved,
        },
        blob_hash: hash_from_columns("attachments", raw.blob_hash_algo, raw.blob_hash)?,
        last_verified_at_ms: raw.last_verified_at_ms,
    })
}

const ATTACHMENT_COLUMNS: &str = "attachment_index, original_name, mime_type, logical_size,
     content_version, telegram_unique_id, telegram_file_id, file_reference,
     availability, can_be_saved, blob_hash_algo, blob_hash, last_verified_at_ms";

impl ReadTxn<'_> {
    /// One attachment by identity.
    pub fn attachment(&self, key: &AttachmentKey) -> Result<Option<AttachmentState>, StateError> {
        let (account_id, namespace) = scope_columns(&key.message.chat.scope);
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {ATTACHMENT_COLUMNS} FROM attachments
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4 AND attachment_index = ?5"
            ))?
            .query_row(
                params![
                    account_id,
                    namespace,
                    key.message.chat.chat_id.0,
                    key.message.message_id.0,
                    i64::from(key.index.0),
                ],
                read_attachment,
            )
            .optional()?;
        raw.map(|raw| finish_attachment(key.message, raw))
            .transpose()
    }

    /// Every attachment of one message, in ordinal order.
    pub fn attachments_of_message(
        &self,
        message: &MessageKey,
    ) -> Result<Vec<AttachmentState>, StateError> {
        let (account_id, namespace) = scope_columns(&message.chat.scope);
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {ATTACHMENT_COLUMNS} FROM attachments
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
               AND message_id = ?4
             ORDER BY attachment_index"
        ))?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                message.chat.chat_id.0,
                message.message_id.0
            ],
            read_attachment,
        )?;
        let mut attachments = Vec::new();
        for row in rows {
            attachments.push(finish_attachment(*message, row?)?);
        }
        Ok(attachments)
    }

    /// Every attachment of one account still referencing the given blob —
    /// "who still needs these bytes" for eviction and dedup (SYNC-052).
    pub fn attachments_referencing_blob(
        &self,
        account: AccountKey,
        hash: &ContentHash,
    ) -> Result<Vec<AttachmentKey>, StateError> {
        let (algo, bytes) = hash_columns(hash);
        let mut statement = self.conn().prepare_cached(
            "SELECT namespace_version, chat_id, message_id, attachment_index FROM attachments
             WHERE account_id = ?1 AND blob_hash_algo = ?2 AND blob_hash = ?3",
        )?;
        let rows = statement.query_map(params![account.account_id.0, algo, bytes], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut keys = Vec::new();
        for row in rows {
            let (namespace, chat_id, message_id, index) = row?;
            let index = u32::try_from(index).map_err(|_| StateError::CorruptRow {
                table: "attachments",
                detail: format!("attachment_index {index} does not fit u32"),
            })?;
            keys.push(AttachmentKey {
                message: MessageKey {
                    chat: ChatKey {
                        scope: AccountScope {
                            account,
                            namespace_version: namespace_from_column("attachments", namespace)?,
                        },
                        chat_id: ChatId(chat_id),
                    },
                    message_id: MessageId(message_id),
                },
                index: AttachmentIndex(index),
            });
        }
        Ok(keys)
    }

    /// One verified blob of an account, or `None` if those bytes were never
    /// completed and verified.
    pub fn blob(
        &self,
        account: AccountKey,
        hash: &ContentHash,
    ) -> Result<Option<BlobRecord>, StateError> {
        let (algo, bytes) = hash_columns(hash);
        let raw: Option<(i64, i64)> = self
            .conn()
            .prepare_cached(
                "SELECT size, first_seen_at_ms FROM blobs
                 WHERE account_id = ?1 AND hash_algo = ?2 AND hash = ?3",
            )?
            .query_row(params![account.account_id.0, algo, bytes], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        raw.map(|(size, first_seen_at_ms)| {
            Ok(BlobRecord {
                hash: *hash,
                size: size_from_column("blobs", size)?,
                first_seen_at_ms,
            })
        })
        .transpose()
    }
}

impl WriteTxn<'_> {
    /// Inserts an attachment or refreshes its metadata and locators,
    /// leaving any verified blob link untouched (SYNC-045).
    ///
    /// The owning message row must exist
    /// ([`WriteTxn::apply_message_changes`]).
    pub fn upsert_attachment(&self, facts: &AttachmentFacts) -> Result<(), StateError> {
        for (value, what) in [
            (
                facts.original_name.as_deref(),
                "attachment original_name must not be empty text",
            ),
            (
                facts.mime_type.as_deref(),
                "attachment mime_type must not be empty text",
            ),
            (
                facts.telegram_unique_id.as_deref(),
                "attachment telegram_unique_id must not be empty text",
            ),
            (
                facts.telegram_file_id.as_deref(),
                "attachment telegram_file_id must not be empty text",
            ),
        ] {
            if value == Some("") {
                return Err(StateError::InvalidArgument { what });
            }
        }
        let (account_id, namespace) = scope_columns(&facts.key.message.chat.scope);
        let logical_size = facts.logical_size.map(size_to_column).transpose()?;
        self.conn()
            .prepare_cached(
                "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                          attachment_index, original_name, mime_type,
                                          logical_size, content_version, telegram_unique_id,
                                          telegram_file_id, file_reference, availability,
                                          can_be_saved)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT (account_id, namespace_version, chat_id, message_id,
                              attachment_index)
                 DO UPDATE SET
                     original_name = excluded.original_name,
                     mime_type = excluded.mime_type,
                     logical_size = excluded.logical_size,
                     content_version = excluded.content_version,
                     telegram_unique_id = excluded.telegram_unique_id,
                     telegram_file_id = excluded.telegram_file_id,
                     file_reference = excluded.file_reference,
                     availability = excluded.availability,
                     can_be_saved = excluded.can_be_saved",
            )?
            .execute(params![
                account_id,
                namespace,
                facts.key.message.chat.chat_id.0,
                facts.key.message.message_id.0,
                i64::from(facts.key.index.0),
                facts.original_name,
                facts.mime_type,
                logical_size,
                facts.content_version.as_str(),
                facts.telegram_unique_id,
                facts.telegram_file_id,
                facts.file_reference,
                facts.availability.as_str(),
                facts.can_be_saved,
            ])?;
        Ok(())
    }

    /// Records a fully downloaded, hash-verified blob (idempotent — the
    /// same bytes recorded twice keep their first-seen time).
    pub fn record_blob(
        &self,
        account: AccountKey,
        hash: &ContentHash,
        size: u64,
        first_seen_at_ms: i64,
    ) -> Result<(), StateError> {
        let (algo, bytes) = hash_columns(hash);
        self.conn()
            .prepare_cached(
                "INSERT INTO blobs (account_id, hash_algo, hash, size, first_seen_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (account_id, hash_algo, hash) DO NOTHING",
            )?
            .execute(params![
                account.account_id.0,
                algo,
                bytes,
                size_to_column(size)?,
                first_seen_at_ms,
            ])?;
        Ok(())
    }

    /// Links verified bytes to an attachment (SYNC-042 promotion step).
    ///
    /// The blob must already be recorded ([`WriteTxn::record_blob`]) and
    /// the attachment must exist — both are [`StateError::RowNotFound`],
    /// not silent creation.
    pub fn link_attachment_blob(
        &self,
        key: &AttachmentKey,
        hash: &ContentHash,
        verified_at_ms: i64,
    ) -> Result<(), StateError> {
        let account = key.message.chat.scope.account;
        if self.read().blob(account, hash)?.is_none() {
            return Err(StateError::RowNotFound { entity: "blob" });
        }
        let (account_id, namespace) = scope_columns(&key.message.chat.scope);
        let (algo, bytes) = hash_columns(hash);
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE attachments
                 SET blob_hash_algo = ?6, blob_hash = ?7, last_verified_at_ms = ?8
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4 AND attachment_index = ?5",
            )?
            .execute(params![
                account_id,
                namespace,
                key.message.chat.chat_id.0,
                key.message.message_id.0,
                i64::from(key.index.0),
                algo,
                bytes,
                verified_at_ms,
            ])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "attachment",
            });
        }
        Ok(())
    }
}
