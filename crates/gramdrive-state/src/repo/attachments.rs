//! Attachment facts and verified blobs (DOM-007, SYNC-045, SYNC-052).
//!
//! Attachment identity is (chat, message, ordinal); Telegram locators are
//! refreshable metadata, never identity. That split is structural here:
//! [`WriteTxn::upsert_attachment`] preserves the blob link when the content
//! version is unchanged, so a locator refresh (SYNC-045) cannot detach
//! verified bytes. A genuine content-version change clears the old link, and
//! [`WriteTxn::link_attachment_blob`] is the only way bytes attach again —
//! after verification, in the transaction that verified them.

use gramdrive_model::attachment::validate_attachment_contract;
use gramdrive_model::identity::{
    AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId, ChatKey,
    ContentHash, ItemId, ItemKey, MessageId, MessageKey,
};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    CacheVerification, ReadTxn, RetentionMode, WriteTxn, hash_columns, hash_from_columns,
    item_id_from_column, namespace_from_column, scope_columns, size_from_column, size_to_column,
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

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $token:literal),+ $(,)? }) => {
        // The concrete values are serialized vocabulary tokens declared at
        // each invocation; `Other` preserves forward-compatible values.
        #[allow(missing_docs)]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $($variant,)+
            Other(String),
        }

        impl $name {
            fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $token,)+
                    Self::Other(value) => value,
                }
            }

            fn parse(value: String) -> Self {
                match value.as_str() {
                    $($token => Self::$variant,)+
                    _ => Self::Other(value),
                }
            }

            /// Stable persisted vocabulary token.
            pub fn tag(&self) -> &str {
                self.as_str()
            }
        }
    };
}

text_enum!(AttachmentLogicalKind {
    Photo => "photo",
    Video => "video",
    Animation => "animation",
    Audio => "audio",
    Voice => "voice",
    VideoNote => "video_note",
    Sticker => "sticker",
    Document => "document",
    OtherMedia => "other_media",
    Unknown => "unknown",
});

text_enum!(TelegramRepresentation {
    OriginalDocument => "original_document",
    Photo => "message_photo",
    Video => "message_video",
    Animation => "message_animation",
    Audio => "message_audio",
    Voice => "message_voice",
    VideoNote => "message_video_note",
    Sticker => "message_sticker",
    UnknownLegacy => "unknown_legacy",
});

text_enum!(AttachmentFidelity {
    Original => "original",
    TelegramVariant => "telegram_variant",
    MetadataOnly => "metadata_only",
    UnknownLegacy => "unknown_legacy",
});

impl AttachmentAvailability {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Fetchable => "fetchable",
            Self::Restricted => "restricted",
            Self::Unavailable => "unavailable",
            Self::ViewOnce => "view_once",
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, StateError> {
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

/// The durable facts of one attachment (domain-model § Attachment).
///
/// Locator fields may change on a reference refresh without changing
/// `content_version`; stable byte/content identity changes advance that
/// version. Nothing here is canonical attachment identity. The blob link is
/// deliberately absent — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFacts {
    /// The attachment's identity.
    pub key: AttachmentKey,
    /// Logical content kind, independent of Telegram representation.
    pub logical_kind: AttachmentLogicalKind,
    /// Telegram message representation used to obtain the bytes.
    pub telegram_representation: TelegramRepresentation,
    /// Fidelity claim for the represented bytes.
    pub fidelity: AttachmentFidelity,
    /// Sender-provided source name, only when Telegram exposes one.
    pub source_name: Option<String>,
    /// MIME type, if known.
    pub mime_type: Option<String>,
    /// Logical size in bytes, if known.
    pub exact_size: Option<u64>,
    /// Version the bytes are fetched under (DOM-003, SYNC-042).
    pub content_version: ContentVersion,
    /// Telegram's stable file identifier, if known.
    pub telegram_unique_id: Option<String>,
    /// TDLib's process-local numeric locator accepted by `downloadFile`.
    pub telegram_local_file_id: Option<i32>,
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

/// Current provider projection facts for one live message attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentProjection {
    /// Canonical durable attachment metadata and verified-blob link.
    pub attachment: AttachmentState,
    /// Absolute Telegram message timestamp used for month placement and the
    /// account-local display-name prefix.
    pub telegram_message_timestamp_ms: i64,
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

/// One superseded allowed attachment version retained prospectively by Audit.
///
/// Historical rows intentionally carry no Telegram download locator. The
/// metadata is durable and already materialized verified bytes may stay
/// owned, but reading this record can never initiate a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedAttachmentVersion {
    /// Canonical attachment item whose live content advanced.
    pub item: ItemId,
    /// Superseded observed byte identity.
    pub content_version: ContentVersion,
    /// Logical kind observed for this version.
    pub logical_kind: AttachmentLogicalKind,
    /// Telegram representation observed for this version.
    pub telegram_representation: TelegramRepresentation,
    /// Fidelity claim observed for this version.
    pub fidelity: AttachmentFidelity,
    /// Sender-provided name, when truthful for the representation.
    pub source_name: Option<String>,
    /// MIME type observed for this version.
    pub mime_type: Option<String>,
    /// Exact logical size, when known.
    pub exact_size: Option<u64>,
    /// Telegram stable file identity, retained as metadata only.
    pub telegram_unique_id: Option<String>,
    /// Verified blob identity, only when bytes were already materialized.
    pub blob_hash: Option<ContentHash>,
    /// When the retained bytes were verified.
    pub last_verified_at_ms: Option<i64>,
    /// Verified on-disk size, only when a materialization remains owned.
    pub materialized_size: Option<u64>,
    /// Opaque on-disk object handle, never a source locator.
    pub materialization_ref: Option<String>,
    /// When the live version was superseded.
    pub retained_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetainedAttachmentMaterialization {
    pub account: AccountKey,
    pub item: ItemId,
    pub content_version: ContentVersion,
    pub reference: String,
}

struct RawAttachment {
    attachment_index: i64,
    logical_kind: String,
    telegram_representation: String,
    fidelity: String,
    source_name: Option<String>,
    mime_type: Option<String>,
    exact_size: Option<i64>,
    content_version: String,
    telegram_unique_id: Option<String>,
    telegram_local_file_id: Option<i32>,
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
        logical_kind: row.get("logical_kind")?,
        telegram_representation: row.get("telegram_representation")?,
        fidelity: row.get("fidelity")?,
        source_name: row.get("source_name")?,
        mime_type: row.get("mime_type")?,
        exact_size: row.get("exact_size")?,
        content_version: row.get("content_version")?,
        telegram_unique_id: row.get("telegram_unique_id")?,
        telegram_local_file_id: row.get("telegram_local_file_id")?,
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
    validate_attachment_contract(
        &raw.telegram_representation,
        &raw.fidelity,
        raw.source_name.as_deref(),
    )
    .map_err(|error| StateError::CorruptRow {
        table: "attachments",
        detail: error.to_string(),
    })?;
    Ok(AttachmentState {
        facts: AttachmentFacts {
            key: AttachmentKey {
                message,
                index: AttachmentIndex(index),
            },
            logical_kind: AttachmentLogicalKind::parse(raw.logical_kind),
            telegram_representation: TelegramRepresentation::parse(raw.telegram_representation),
            fidelity: AttachmentFidelity::parse(raw.fidelity),
            source_name: raw.source_name,
            mime_type: raw.mime_type,
            exact_size: raw
                .exact_size
                .map(|size| size_from_column("attachments", size))
                .transpose()?,
            content_version: ContentVersion::new(raw.content_version).map_err(|error| {
                StateError::CorruptRow {
                    table: "attachments",
                    detail: format!("content_version does not parse: {error}"),
                }
            })?,
            telegram_unique_id: raw.telegram_unique_id,
            telegram_local_file_id: raw.telegram_local_file_id,
            telegram_file_id: raw.telegram_file_id,
            file_reference: raw.file_reference,
            availability: AttachmentAvailability::parse(&raw.availability)?,
            can_be_saved: raw.can_be_saved,
        },
        blob_hash: hash_from_columns("attachments", raw.blob_hash_algo, raw.blob_hash)?,
        last_verified_at_ms: raw.last_verified_at_ms,
    })
}

const ATTACHMENT_COLUMNS: &str = "attachment_index, logical_kind, telegram_representation,
     fidelity, source_name, mime_type, exact_size,
     content_version, telegram_unique_id, telegram_file_id, file_reference,
     telegram_local_file_id, availability, can_be_saved, blob_hash_algo, blob_hash,
     last_verified_at_ms";

const JOINED_ATTACHMENT_COLUMNS: &str = "
     a.attachment_index AS attachment_index,
     a.logical_kind AS logical_kind,
     a.telegram_representation AS telegram_representation,
     a.fidelity AS fidelity,
     a.source_name AS source_name,
     a.mime_type AS mime_type,
     a.exact_size AS exact_size,
     a.content_version AS content_version,
     a.telegram_unique_id AS telegram_unique_id,
     a.telegram_file_id AS telegram_file_id,
     a.file_reference AS file_reference,
     a.telegram_local_file_id AS telegram_local_file_id,
     a.availability AS availability,
     a.can_be_saved AS can_be_saved,
     a.blob_hash_algo AS blob_hash_algo,
     a.blob_hash AS blob_hash,
     a.last_verified_at_ms AS last_verified_at_ms";

impl ReadTxn<'_> {
    /// Attachment identities that currently own verified bytes for one
    /// account, ordered by namespace/chat/message/index.
    ///
    /// Policy enforcement must scale with materialized media, not with every
    /// metadata-only attachment discovered during account-wide history.
    /// `attachments_by_blob` makes this a bounded partial scan in the common
    /// dataless case.
    pub fn materialized_attachment_keys(
        &self,
        account: AccountKey,
    ) -> Result<Vec<AttachmentKey>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT namespace_version, chat_id, message_id, attachment_index
             FROM attachments
             WHERE account_id = ?1 AND blob_hash IS NOT NULL
             ORDER BY namespace_version, chat_id, message_id, attachment_index",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
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

    /// Every canonical attachment retained for one account, ordered by
    /// namespace, chat, message, and attachment ordinal.
    ///
    /// This is the bounded local-policy scan used when an authoritative
    /// Telegram restriction must detach bytes after they were materialized.
    /// It performs no source access and includes Audit-retained deleted
    /// metadata so the caller can distinguish deletion from protection.
    pub fn attachments(&self, account: AccountKey) -> Result<Vec<AttachmentState>, StateError> {
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {ATTACHMENT_COLUMNS}, namespace_version, chat_id, message_id
             FROM attachments
             WHERE account_id = ?1
             ORDER BY namespace_version, chat_id, message_id, attachment_index"
        ))?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            Ok((
                read_attachment(row)?,
                row.get::<_, i64>("namespace_version")?,
                row.get::<_, i64>("chat_id")?,
                row.get::<_, i64>("message_id")?,
            ))
        })?;
        let mut attachments = Vec::new();
        for row in rows {
            let (raw, namespace, chat_id, message_id) = row?;
            attachments.push(finish_attachment(
                MessageKey {
                    chat: ChatKey {
                        scope: AccountScope {
                            account,
                            namespace_version: namespace_from_column("attachments", namespace)?,
                        },
                        chat_id: ChatId(chat_id),
                    },
                    message_id: MessageId(message_id),
                },
                raw,
            )?);
        }
        Ok(attachments)
    }

    /// Current attachments of live messages in one chat, ordered by Telegram
    /// timestamp, message identity, and attachment ordinal.
    pub fn attachment_projections_of_chat(
        &self,
        chat: &ChatKey,
    ) -> Result<Vec<AttachmentProjection>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {JOINED_ATTACHMENT_COLUMNS}, m.message_id, m.sent_at_ms
             FROM attachments a
             JOIN messages m
               ON m.account_id = a.account_id
              AND m.namespace_version = a.namespace_version
              AND m.chat_id = a.chat_id
              AND m.message_id = a.message_id
             WHERE a.account_id = ?1 AND a.namespace_version = ?2 AND a.chat_id = ?3
               AND m.is_deleted = 0
             ORDER BY m.sent_at_ms, m.message_id, a.attachment_index"
        ))?;
        let rows = statement.query_map(params![account_id, namespace, chat.chat_id.0], |row| {
            Ok((
                read_attachment(row)?,
                row.get::<_, i64>("message_id")?,
                row.get::<_, i64>("sent_at_ms")?,
            ))
        })?;
        let mut projections = Vec::new();
        for row in rows {
            let (raw, message_id, sent_at_ms) = row?;
            projections.push(AttachmentProjection {
                attachment: finish_attachment(
                    MessageKey {
                        chat: *chat,
                        message_id: MessageId(message_id),
                    },
                    raw,
                )?,
                telegram_message_timestamp_ms: sent_at_ms,
            });
        }
        Ok(projections)
    }

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

    /// Audit-retained superseded versions of one canonical attachment,
    /// ordered by observation time and content version.
    pub fn retained_attachment_versions(
        &self,
        key: &AttachmentKey,
    ) -> Result<Vec<RetainedAttachmentVersion>, StateError> {
        let account = key.message.chat.scope.account;
        let item = ItemKey::Canonical(CanonicalKey::Attachment(*key)).id();
        let mut statement = self.conn().prepare_cached(
            "SELECT content_version, logical_kind, telegram_representation, fidelity,
                    source_name, mime_type, exact_size, telegram_unique_id,
                    blob_hash_algo, blob_hash, last_verified_at_ms, materialized_size,
                    materialization_ref, retained_at_ms
             FROM retained_attachment_versions
             WHERE account_id = ?1 AND item_id = ?2
             ORDER BY retained_at_ms, content_version",
        )?;
        let rows = statement.query_map(params![account.account_id.0, item.as_bytes()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<Vec<u8>>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, i64>(13)?,
            ))
        })?;
        let mut versions = Vec::new();
        for row in rows {
            let (
                content_version,
                logical_kind,
                representation,
                fidelity,
                source_name,
                mime_type,
                exact_size,
                telegram_unique_id,
                blob_hash_algo,
                blob_hash,
                last_verified_at_ms,
                materialized_size,
                materialization_ref,
                retained_at_ms,
            ) = row?;
            validate_attachment_contract(&representation, &fidelity, source_name.as_deref())
                .map_err(|error| StateError::CorruptRow {
                    table: "retained_attachment_versions",
                    detail: error.to_string(),
                })?;
            versions.push(RetainedAttachmentVersion {
                item: item.clone(),
                content_version: ContentVersion::new(content_version).map_err(|error| {
                    StateError::CorruptRow {
                        table: "retained_attachment_versions",
                        detail: format!("content_version does not parse: {error}"),
                    }
                })?,
                logical_kind: AttachmentLogicalKind::parse(logical_kind),
                telegram_representation: TelegramRepresentation::parse(representation),
                fidelity: AttachmentFidelity::parse(fidelity),
                source_name,
                mime_type,
                exact_size: exact_size
                    .map(|size| size_from_column("retained_attachment_versions", size))
                    .transpose()?,
                telegram_unique_id,
                blob_hash: hash_from_columns(
                    "retained_attachment_versions",
                    blob_hash_algo,
                    blob_hash,
                )?,
                last_verified_at_ms,
                materialized_size: materialized_size
                    .map(|size| size_from_column("retained_attachment_versions", size))
                    .transpose()?,
                materialization_ref,
                retained_at_ms,
            });
        }
        Ok(versions)
    }

    /// Canonical attachments with at least one Audit-retained historical
    /// version for one account.
    ///
    /// This includes detached histories whose live `attachments` row was
    /// removed by a later Audit observation. Policy enforcement uses the
    /// decoded canonical key to re-check chat and item restrictions without
    /// retaining or reconstructing any source locator.
    pub fn retained_attachment_keys(
        &self,
        account: AccountKey,
    ) -> Result<Vec<AttachmentKey>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT DISTINCT item_id
             FROM retained_attachment_versions
             WHERE account_id = ?1
             ORDER BY item_id",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut keys = Vec::new();
        for row in rows {
            let item = item_id_from_column("retained_attachment_versions", &row?)?;
            let ItemKey::Canonical(CanonicalKey::Attachment(key)) = item.key() else {
                return Err(StateError::CorruptRow {
                    table: "retained_attachment_versions",
                    detail: "item_id is not a canonical attachment identity".to_owned(),
                });
            };
            if key.message.chat.scope.account != account {
                return Err(StateError::CorruptRow {
                    table: "retained_attachment_versions",
                    detail: "item_id account does not match account_id".to_owned(),
                });
            }
            keys.push(key);
        }
        Ok(keys)
    }

    pub(crate) fn retained_attachment_materializations(
        &self,
    ) -> Result<Vec<RetainedAttachmentMaterialization>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT account_id, item_id, content_version, materialization_ref
             FROM retained_attachment_versions
             WHERE materialization_ref IS NOT NULL
             ORDER BY account_id, item_id, content_version",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut materializations = Vec::new();
        for row in rows {
            let (account_id, item, content_version, reference) = row?;
            materializations.push(RetainedAttachmentMaterialization {
                account: AccountKey {
                    account_id: gramdrive_model::identity::AccountId(account_id),
                },
                item: item_id_from_column("retained_attachment_versions", &item)?,
                content_version: ContentVersion::new(content_version).map_err(|error| {
                    StateError::CorruptRow {
                        table: "retained_attachment_versions",
                        detail: format!("content_version does not parse: {error}"),
                    }
                })?,
                reference,
            });
        }
        Ok(materializations)
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
    /// Permanently redacts every attachment locator and descriptive byte fact
    /// owned by a protected chat.
    ///
    /// Logical kind/representation remain so the namespace can expose an
    /// honest unavailable placeholder. The deterministic restricted version
    /// prevents a later protection removal from reviving a pre-restriction
    /// provider content version.
    pub fn redact_protected_chat_attachments(&self, chat: &ChatKey) -> Result<usize, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE attachments
                 SET source_name = NULL, mime_type = NULL, exact_size = NULL,
                     content_version = 'restricted-attachment-v1/'
                         || CAST(chat_id AS TEXT) || '/'
                         || CAST(message_id AS TEXT) || '/'
                         || CAST(attachment_index AS TEXT),
                     telegram_unique_id = NULL, telegram_local_file_id = NULL,
                     telegram_file_id = NULL, file_reference = NULL,
                     availability = 'restricted', can_be_saved = 0,
                     blob_hash_algo = NULL, blob_hash = NULL,
                     last_verified_at_ms = NULL
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND (
                       source_name IS NOT NULL OR mime_type IS NOT NULL OR exact_size IS NOT NULL
                       OR telegram_unique_id IS NOT NULL OR telegram_local_file_id IS NOT NULL
                       OR telegram_file_id IS NOT NULL OR file_reference IS NOT NULL
                       OR availability <> 'restricted' OR can_be_saved <> 0
                       OR blob_hash IS NOT NULL
                       OR content_version <> 'restricted-attachment-v1/'
                           || CAST(chat_id AS TEXT) || '/'
                           || CAST(message_id AS TEXT) || '/'
                           || CAST(attachment_index AS TEXT)
                   )",
            )?
            .execute(params![account_id, namespace, chat.chat_id.0])?;
        Ok(changed)
    }

    /// Reconciles the current attachment set of one live message.
    ///
    /// Existing identities are upserted first so locator refreshes preserve a
    /// verified blob link. Identities absent from the accepted current payload
    /// are then removed; historical event payloads remain available to Audit
    /// rendering independently of this current-state projection.
    pub fn replace_message_attachments(
        &self,
        message: &MessageKey,
        facts: &[AttachmentFacts],
        observed_at_ms: i64,
    ) -> Result<(), StateError> {
        let retention = self
            .read()
            .retention_mode(message.chat.scope.account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        let chat_is_protected = self
            .read()
            .chat(&message.chat)?
            .ok_or(StateError::RowNotFound { entity: "chat" })?
            .is_protected;
        for (position, fact) in facts.iter().enumerate() {
            if fact.key.message != *message {
                return Err(StateError::InvalidArgument {
                    what: "replacement attachment belongs to another message",
                });
            }
            if facts[..position]
                .iter()
                .any(|prior| prior.key.index == fact.key.index)
            {
                return Err(StateError::InvalidArgument {
                    what: "replacement attachment indices must be unique",
                });
            }
            let current = self.read().attachment(&fact.key)?;
            let version_changed = current
                .as_ref()
                .is_some_and(|stored| stored.facts.content_version != fact.content_version);
            if chat_is_protected
                || !fact.can_be_saved
                || fact.availability != AttachmentAvailability::Fetchable
            {
                self.purge_attachment_materialization(&fact.key, observed_at_ms)?;
            } else if version_changed {
                match retention {
                    RetentionMode::Mirror => {
                        self.purge_attachment_materialization(&fact.key, observed_at_ms)?;
                    }
                    RetentionMode::Audit => {
                        if let Some(current) = current.as_ref() {
                            self.retain_superseded_attachment(current, observed_at_ms)?;
                        }
                    }
                }
            }
            self.upsert_attachment(fact)?;
        }
        let current = self.read().attachments_of_message(message)?;
        let (account_id, namespace) = scope_columns(&message.chat.scope);
        for attachment in current {
            if facts
                .iter()
                .any(|fact| fact.key.index == attachment.facts.key.index)
            {
                continue;
            }
            match retention {
                RetentionMode::Mirror => {
                    self.purge_attachment_materialization(&attachment.facts.key, observed_at_ms)?;
                }
                RetentionMode::Audit => {
                    self.retain_superseded_attachment(&attachment, observed_at_ms)?;
                }
            }
            self.conn()
                .prepare_cached(
                    "DELETE FROM attachments
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                       AND message_id = ?4 AND attachment_index = ?5",
                )?
                .execute(params![
                    account_id,
                    namespace,
                    message.chat.chat_id.0,
                    message.message_id.0,
                    i64::from(attachment.facts.key.index.0),
                ])?;
        }
        Ok(())
    }

    /// Releases every materialized-byte owner for one attachment and journals
    /// its physical object before the attachment is removed or becomes
    /// authoritatively unavailable.
    ///
    /// The queue, cache/pin ownership, verified attachment link, and orphan
    /// blob cleanup share the caller's transaction. Replaying the same source
    /// observation is therefore harmless, while a crash after commit leaves a
    /// durable file-deletion row for the hydrator startup repair.
    pub(crate) fn purge_attachment_materialization(
        &self,
        key: &AttachmentKey,
        queued_at_ms: i64,
    ) -> Result<(), StateError> {
        let account = key.message.chat.scope.account;
        let item = ItemKey::Canonical(CanonicalKey::Attachment(*key)).id();
        self.queue_retained_attachment_purge(account, &item, queued_at_ms)?;
        self.queue_restricted_cache_purge(account, &item, queued_at_ms)?;
        let (account_id, namespace) = scope_columns(&key.message.chat.scope);
        self.conn()
            .prepare_cached(
                "UPDATE attachments
                 SET blob_hash_algo = NULL, blob_hash = NULL, last_verified_at_ms = NULL
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4 AND attachment_index = ?5",
            )?
            .execute(params![
                account_id,
                namespace,
                key.message.chat.chat_id.0,
                key.message.message_id.0,
                i64::from(key.index.0),
            ])?;
        self.purge_unreferenced_blobs(account)?;
        Ok(())
    }

    fn retain_superseded_attachment(
        &self,
        current: &AttachmentState,
        retained_at_ms: i64,
    ) -> Result<(), StateError> {
        let account = current.facts.key.message.chat.scope.account;
        let item = ItemKey::Canonical(CanonicalKey::Attachment(current.facts.key)).id();
        let cache = self.read().cache_entry(&item)?;
        let materialized = cache.filter(|entry| {
            entry.content_version == current.facts.content_version
                && entry.verification == CacheVerification::Verified
                && entry.blob_hash.is_some()
                && entry.materialization_ref.is_some()
        });
        let (blob_hash, materialized_size, materialization_ref) =
            materialized.as_ref().map_or((None, None, None), |entry| {
                (
                    entry.blob_hash,
                    Some(entry.size),
                    entry.materialization_ref.clone(),
                )
            });
        let (blob_hash_algo, blob_hash_bytes) = blob_hash
            .as_ref()
            .map(hash_columns)
            .map_or((None, None), |(algo, bytes)| (Some(algo), Some(bytes)));
        self.conn()
            .prepare_cached(
                "INSERT INTO retained_attachment_versions (
                     account_id, item_id, content_version, logical_kind,
                     telegram_representation, fidelity, source_name, mime_type,
                     exact_size, telegram_unique_id, blob_hash_algo, blob_hash,
                     last_verified_at_ms, materialized_size, materialization_ref,
                     retained_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16)
                 ON CONFLICT (account_id, item_id, content_version) DO UPDATE SET
                     source_name = excluded.source_name,
                     mime_type = excluded.mime_type,
                     exact_size = excluded.exact_size,
                     telegram_unique_id = excluded.telegram_unique_id,
                     blob_hash_algo = COALESCE(
                         retained_attachment_versions.blob_hash_algo,
                         excluded.blob_hash_algo
                     ),
                     blob_hash = COALESCE(
                         retained_attachment_versions.blob_hash,
                         excluded.blob_hash
                     ),
                     last_verified_at_ms = COALESCE(
                         retained_attachment_versions.last_verified_at_ms,
                         excluded.last_verified_at_ms
                     ),
                     materialized_size = COALESCE(
                         retained_attachment_versions.materialized_size,
                         excluded.materialized_size
                     ),
                     materialization_ref = COALESCE(
                         retained_attachment_versions.materialization_ref,
                         excluded.materialization_ref
                     ),
                     retained_at_ms = MIN(
                         retained_attachment_versions.retained_at_ms,
                         excluded.retained_at_ms
                     )",
            )?
            .execute(params![
                account.account_id.0,
                item.as_bytes(),
                current.facts.content_version.as_str(),
                current.facts.logical_kind.as_str(),
                current.facts.telegram_representation.as_str(),
                current.facts.fidelity.as_str(),
                current.facts.source_name,
                current.facts.mime_type,
                current.facts.exact_size.map(size_to_column).transpose()?,
                current.facts.telegram_unique_id,
                blob_hash_algo,
                blob_hash_bytes,
                materialized.as_ref().and(current.last_verified_at_ms),
                materialized_size.map(size_to_column).transpose()?,
                materialization_ref,
                retained_at_ms,
            ])?;
        if materialized.is_some() {
            self.conn()
                .prepare_cached(
                    "DELETE FROM cache_entries
                     WHERE item_id = ?1 AND account_id = ?2 AND content_version = ?3",
                )?
                .execute(params![
                    item.as_bytes(),
                    account.account_id.0,
                    current.facts.content_version.as_str()
                ])?;
        }
        Ok(())
    }

    /// Removes every Audit-retained historical version of one attachment and
    /// journals each materialized object in the same transaction.
    ///
    /// The caller supplies the account independently so a malformed or
    /// cross-account item cannot release another account's ownership.
    /// Repeating the operation is idempotent.
    pub fn queue_retained_attachment_purge(
        &self,
        account: AccountKey,
        item: &ItemId,
        queued_at_ms: i64,
    ) -> Result<usize, StateError> {
        let references = {
            let mut statement = self.conn().prepare_cached(
                "SELECT materialization_ref FROM retained_attachment_versions
                 WHERE account_id = ?1 AND item_id = ?2
                   AND materialization_ref IS NOT NULL",
            )?;
            let rows = statement
                .query_map(params![account.account_id.0, item.as_bytes()], |row| {
                    row.get::<_, String>(0)
                })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for reference in &references {
            self.conn()
                .prepare_cached(
                    "INSERT INTO retention_purge_queue (
                         account_id, materialization_ref, queued_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (account_id, materialization_ref) DO NOTHING",
                )?
                .execute(params![account.account_id.0, reference, queued_at_ms])?;
        }
        let removed = self
            .conn()
            .prepare_cached(
                "DELETE FROM retained_attachment_versions
                 WHERE account_id = ?1 AND item_id = ?2",
            )?
            .execute(params![account.account_id.0, item.as_bytes()])?;
        Ok(removed)
    }

    pub(crate) fn clear_retained_attachment_materialization(
        &self,
        account: AccountKey,
        item: &ItemId,
        content_version: &ContentVersion,
    ) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE retained_attachment_versions
                 SET blob_hash_algo = NULL, blob_hash = NULL,
                     last_verified_at_ms = NULL, materialized_size = NULL,
                     materialization_ref = NULL
                 WHERE account_id = ?1 AND item_id = ?2 AND content_version = ?3",
            )?
            .execute(params![
                account.account_id.0,
                item.as_bytes(),
                content_version.as_str()
            ])?;
        Ok(changed > 0)
    }

    /// Inserts an attachment or refreshes its metadata and locators.
    ///
    /// A verified blob link is preserved exactly when `content_version` is
    /// unchanged (SYNC-045). A new content version clears the old verified
    /// link so stale bytes cannot be served under the replacement version.
    ///
    /// The owning message row must exist
    /// ([`WriteTxn::apply_message_changes`]).
    pub fn upsert_attachment(&self, facts: &AttachmentFacts) -> Result<(), StateError> {
        for (value, what) in [
            (
                facts.source_name.as_deref(),
                "attachment source_name must not be empty text",
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
        if facts.logical_kind.as_str().is_empty()
            || facts.telegram_representation.as_str().is_empty()
            || facts.fidelity.as_str().is_empty()
        {
            return Err(StateError::InvalidArgument {
                what: "attachment kind, representation, and fidelity must not be empty",
            });
        }
        validate_attachment_contract(
            facts.telegram_representation.as_str(),
            facts.fidelity.as_str(),
            facts.source_name.as_deref(),
        )
        .map_err(|_| StateError::InvalidArgument {
            what: "attachment representation, fidelity, and source_name are not truthful",
        })?;
        let exact_size = facts.exact_size.map(size_to_column).transpose()?;
        self.conn()
            .prepare_cached(
                "INSERT INTO attachments (account_id, namespace_version, chat_id, message_id,
                                          attachment_index, logical_kind,
                                          telegram_representation, fidelity, source_name, mime_type,
                                          exact_size, content_version, telegram_unique_id,
                                          telegram_file_id, file_reference,
                                          telegram_local_file_id, availability, can_be_saved)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT (account_id, namespace_version, chat_id, message_id,
                              attachment_index)
                 DO UPDATE SET
                     logical_kind = excluded.logical_kind,
                     telegram_representation = excluded.telegram_representation,
                     fidelity = excluded.fidelity,
                     source_name = excluded.source_name,
                     mime_type = excluded.mime_type,
                     exact_size = excluded.exact_size,
                     content_version = excluded.content_version,
                     telegram_unique_id = excluded.telegram_unique_id,
                     telegram_file_id = excluded.telegram_file_id,
                     file_reference = excluded.file_reference,
                     telegram_local_file_id = excluded.telegram_local_file_id,
                     availability = excluded.availability,
                     can_be_saved = excluded.can_be_saved,
                     blob_hash_algo = CASE
                         WHEN attachments.content_version = excluded.content_version
                         THEN attachments.blob_hash_algo ELSE NULL END,
                     blob_hash = CASE
                         WHEN attachments.content_version = excluded.content_version
                         THEN attachments.blob_hash ELSE NULL END,
                     last_verified_at_ms = CASE
                         WHEN attachments.content_version = excluded.content_version
                         THEN attachments.last_verified_at_ms ELSE NULL END",
            )?
            .execute(params![
                account_id,
                namespace,
                facts.key.message.chat.chat_id.0,
                facts.key.message.message_id.0,
                i64::from(facts.key.index.0),
                facts.logical_kind.as_str(),
                facts.telegram_representation.as_str(),
                facts.fidelity.as_str(),
                facts.source_name,
                facts.mime_type,
                exact_size,
                facts.content_version.as_str(),
                facts.telegram_unique_id,
                facts.telegram_file_id,
                facts.file_reference,
                facts.telegram_local_file_id,
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

    /// Detaches verified bytes from one attachment after an authoritative
    /// source restriction. Returns the prior blob identity when one existed.
    ///
    /// The blob row is deliberately not removed here: another attachment,
    /// story, or cache entry may still own the same content-addressed bytes.
    /// Call [`WriteTxn::purge_unreferenced_blobs`] after all links in the
    /// policy batch have been detached.
    pub fn unlink_attachment_blob(
        &self,
        key: &AttachmentKey,
    ) -> Result<Option<ContentHash>, StateError> {
        let prior = self
            .read()
            .attachment(key)?
            .and_then(|state| state.blob_hash);
        if prior.is_none() {
            return Ok(None);
        }
        let (account_id, namespace) = scope_columns(&key.message.chat.scope);
        self.conn()
            .prepare_cached(
                "UPDATE attachments
                 SET blob_hash_algo = NULL, blob_hash = NULL, last_verified_at_ms = NULL
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4 AND attachment_index = ?5",
            )?
            .execute(params![
                account_id,
                namespace,
                key.message.chat.chat_id.0,
                key.message.message_id.0,
                i64::from(key.index.0),
            ])?;
        Ok(prior)
    }

    /// Removes verified blob rows no attachment, story, or cache entry still
    /// owns. Physical-object deletion is journalled separately from cache-row
    /// removal, so this operation remains purely transactional.
    pub fn purge_unreferenced_blobs(&self, account: AccountKey) -> Result<usize, StateError> {
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
            .execute(params![account.account_id.0])
            .map_err(StateError::from)
    }
}
