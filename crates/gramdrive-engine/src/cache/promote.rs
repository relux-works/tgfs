//! The promotion step: verify, materialize, publish. Module-level rationale
//! and the crash-safety ordering are in [`super`].

use std::num::NonZeroU64;

use gramdrive_model::hash::Sha256;
use gramdrive_model::identity::{
    AccountKey, AttachmentKey, CanonicalKey, ContentHash, ItemId, ItemKey,
};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    CacheEntryRecord, CacheKind, CacheVerification, FailureCategory, TransferId, TransferRecord,
};

use crate::fetch::{StagingError, StagingHost};
use crate::transfer::{EngineError, ItemStanding, StagingDisposal, item_standing, ranges};

/// Default verification read grain: 256 KiB, big enough to keep the hash loop
/// off the per-call overhead and small enough to bound the transient buffer
/// for a multi-gigabyte object. It only sizes the read buffer — the digest is
/// identical for any grain (`gramdrive_model::hash` is chunk-independent).
const DEFAULT_READ_CHUNK_BYTES: u64 = 256 * 1024;

/// The host's content-addressed cache store — the atomic promotion of a
/// verified staging object into durable cache storage.
///
/// The engine is platform-neutral (crates/README.md) and cannot open a file,
/// so *where* cache bytes live and *how* a rename is made durable is the
/// embedding host's decision; this port is the seam. It is the file-before-row
/// half of the crash-safety contract (SYNC-053) — the engine commits the
/// `cache_entries` row only after [`PromotionHost::promote`] returns.
///
/// # Contract
///
/// - **Content-addressed.** The returned handle is a deterministic function of
///   `hash`: identical content promotes to the same handle. This is what makes
///   a duplicate a rename onto an existing name and lets one on-disk object
///   back several cache entries (dedup, SYNC-052).
/// - **Idempotent.** Promoting a `hash` whose object already exists succeeds
///   without moving anything, returns the same handle, and reports
///   [`Materialization::deduplicated`]. The now-redundant `staging` object is
///   the host's to discard.
/// - **Ownership.** On success the host owns the `staging` object: it either
///   renamed it into place or, on a dedup hit, may drop it. The engine does
///   not delete staging after a successful promote; the terminal transfer's
///   now-stale `temp_ref` names nothing a live transfer claims, so it is inert
///   to reconciliation.
/// - **Durable before return (fsync policy).** Once this returns, a crash must
///   not lose the object the handle names. Concretely, on a POSIX host: flush
///   the staging file's data (`fsync` the file), `rename` it onto the final
///   content-addressed path (atomic within a filesystem), then `fsync` the
///   containing directory so the new name itself survives. A dedup hit needs
///   none of this — the durable object already exists.
/// - **`staging` is `None`** only for the zero-byte object, whose content is
///   empty and needs no staging area: the host materializes an empty object
///   for the empty-content hash.
pub trait PromotionHost: Send {
    /// Atomically promotes `staging` into content-addressed cache storage
    /// under `hash`, returning the handle the cache object is now reachable by.
    fn promote(
        &mut self,
        staging: Option<&str>,
        hash: &ContentHash,
    ) -> Result<Materialization, PromotionHostError>;
}

/// The outcome of a successful [`PromotionHost::promote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialization {
    /// The opaque `materialization_ref` the cache object is reachable by.
    pub reference: String,
    /// Whether the content-addressed object already existed, so nothing was
    /// moved (SYNC-052 dedup).
    pub deduplicated: bool,
}

/// Why a [`PromotionHost::promote`] failed. Opaque like
/// [`gramdrive_state::StorageError`]: the reason a host could not materialize
/// bytes is its own vocabulary, and the engine can only carry its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionHostError {
    /// The host's own description of the failure.
    pub detail: String,
}

impl PromotionHostError {
    /// A failure described by `detail`.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PromotionHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PromotionHostError {}

/// Tuning for the promotion pass. Pure policy — no durable state — so changing
/// it between runs is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromotionConfig {
    /// Read grain for hashing the staged object; see
    /// [`DEFAULT_READ_CHUNK_BYTES`].
    pub read_chunk_bytes: NonZeroU64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            read_chunk_bytes: NonZeroU64::new(DEFAULT_READ_CHUNK_BYTES).unwrap_or(NonZeroU64::MIN),
        }
    }
}

/// What [`Promoter::promote`] resolved a finished transfer to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a promotion outcome may carry a staging disposal the host must honor"]
pub enum Promotion {
    /// Bytes verified, materialized, and published. The cache now serves the
    /// item; the blob is recorded and, for an attachment, linked. The staging
    /// object was consumed by the promote (or, on `deduplicated`, is the
    /// host's to drop) — no disposal is owed here.
    Materialized {
        /// The content identity of the promoted bytes.
        hash: ContentHash,
        /// Size in bytes of the complete content.
        size: u64,
        /// The `materialization_ref` the cache object is reachable by.
        materialization_ref: String,
        /// Whether the content-addressed object already existed (dedup).
        deduplicated: bool,
        /// Whether an attachment row was linked to the blob (false for a
        /// non-attachment item, or an attachment the projection has not
        /// recorded yet).
        attachment_linked: bool,
    },
    /// A verified cache entry for this item at this version already exists:
    /// the promotion already ran. Idempotent no-op — the staging object, if
    /// any, was consumed by the first promotion and is not touched.
    AlreadyMaterialized {
        /// The blob hash the existing entry records, if it materializes one.
        hash: Option<ContentHash>,
        /// The existing entry's materialization handle.
        materialization_ref: Option<String>,
    },
    /// The staged bytes did not verify — unreadable, or short of the item's
    /// extent (truncation). Failed closed: nothing published, and the staging
    /// object holds untrusted bytes the host must drop (SYNC-042).
    IntegrityFailed {
        /// What was wrong, for diagnostics.
        detail: String,
        /// The untrusted staging area to dispose.
        disposal: Option<StagingDisposal>,
    },
    /// The pinned content version departed before publication: bytes fetched
    /// for the old version are not published (SYNC-042).
    VersionDeparted {
        /// How the departure classifies.
        category: FailureCategory,
        /// The staging area to dispose. `None` when the version left in the
        /// window *after* the file promote — the materialized object is then
        /// an orphan reconciliation reclaims, and the staging is already
        /// consumed.
        disposal: Option<StagingDisposal>,
    },
    /// The transfer completed a partial range, not the whole object: there is
    /// no blob to materialize (domain-model § Blob). The staged bytes served
    /// readers already; the host disposes them.
    NotWholeContent {
        /// The staging area to dispose.
        disposal: Option<StagingDisposal>,
    },
}

/// Verifies and promotes finished transfers into content-addressed cache.
///
/// Stateless over the store on purpose, like [`TransferMachine`]: the durable
/// rows and the on-disk objects are the only authoritative state, so a host
/// constructs one per policy and passes the [`StateStore`] into each call.
///
/// [`TransferMachine`]: crate::transfer::TransferMachine
#[derive(Debug, Clone, Default)]
pub struct Promoter {
    config: PromotionConfig,
}

impl Promoter {
    /// A promoter applying `config`.
    pub fn new(config: PromotionConfig) -> Self {
        Self { config }
    }

    /// The policy this promoter applies.
    pub fn config(&self) -> &PromotionConfig {
        &self.config
    }

    /// Verifies the bytes a finished `transfer` staged and, if they prove
    /// complete and correct, materializes them into content-addressed cache —
    /// see [`super`] for the ordering and its crash-safety rationale.
    ///
    /// Call it on a transfer the machine already carried to
    /// [`CompleteOutcome::Promoted`](crate::transfer::CompleteOutcome::Promoted):
    /// the row is terminal `done`, so nothing else is writing its staging.
    /// `now_ms` stamps the rows it writes (the core reads no clock, SYNC-073).
    pub fn promote(
        &self,
        store: &mut StateStore,
        staging_host: &mut dyn StagingHost,
        promotion_host: &mut dyn PromotionHost,
        transfer: TransferId,
        now_ms: i64,
    ) -> Result<Promotion, EngineError> {
        let record = {
            let read = store.read_txn()?;
            read.transfer(transfer)?
                .ok_or(gramdrive_state::StateError::RowNotFound { entity: "transfer" })?
        };
        let item = record.item.clone();
        let pinned = record.content_version.clone();

        // Standing at the top: it both catches a departed pin before any work
        // and yields the extent the completeness gate and the hash loop need.
        let standing = {
            let read = store.read_txn()?;
            item_standing(&read, &item, &pinned)?
        };
        let extent = match standing {
            ItemStanding::Departed { category } => {
                return Ok(Promotion::VersionDeparted {
                    category,
                    disposal: staging_disposal(&record),
                });
            }
            ItemStanding::Pinned { extent } => extent,
        };

        // A blob is whole content. A partial range, or content whose extent
        // the projection still does not know, is not a blob to materialize.
        let Some(extent) = extent else {
            return Ok(Promotion::NotWholeContent {
                disposal: staging_disposal(&record),
            });
        };
        let whole = ranges::whole_object(extent);
        if !ranges::subtract(&whole, &record.completed_ranges).is_empty() {
            return Ok(Promotion::NotWholeContent {
                disposal: staging_disposal(&record),
            });
        }

        // Idempotency: a verified entry for this item at this version is proof
        // the promotion already ran. The standing check above guarantees the
        // current version is `pinned`, so a match here is this exact work.
        let existing = {
            let read = store.read_txn()?;
            read.cache_entry(&item)?
        };
        if let Some(existing) = existing
            && existing.content_version == pinned
            && existing.verification == CacheVerification::Verified
        {
            return Ok(Promotion::AlreadyMaterialized {
                hash: existing.blob_hash,
                materialization_ref: existing.materialization_ref,
            });
        }

        // Verify: hash the whole staged object, failing closed on any byte we
        // cannot read (a short object is truncation).
        let hash =
            match self.hash_staged(staging_host, transfer, record.temp_ref.as_deref(), extent) {
                Ok(hash) => hash,
                Err(detail) => {
                    return Ok(Promotion::IntegrityFailed {
                        detail,
                        disposal: staging_disposal(&record),
                    });
                }
            };

        // File before row: the host makes the object durable, then we record
        // it. A crash in between leaves an orphan reconciliation reclaims.
        let materialization = promotion_host
            .promote(record.temp_ref.as_deref(), &hash)
            .map_err(|error| EngineError::Storage {
                detail: error.detail,
            })?;

        self.publish(store, &record, &hash, extent, materialization, now_ms)
    }

    /// The publishing transaction (step 4): re-check the pin under the same
    /// snapshot as the writes, then record blob + cache entry + attachment
    /// link atomically.
    fn publish(
        &self,
        store: &mut StateStore,
        record: &TransferRecord,
        hash: &ContentHash,
        size: u64,
        materialization: Materialization,
        now_ms: i64,
    ) -> Result<Promotion, EngineError> {
        let item = &record.item;
        let canonical = canonical_key(item);
        let account = canonical_account(&canonical);
        let tx = store.write_txn()?;

        // Re-check the pin inside the publishing transaction (SYNC-042): if it
        // left in the window after the file promote, the materialized object
        // is an orphan reconciliation reclaims — we simply do not record it.
        if let ItemStanding::Departed { category } =
            item_standing(tx.read(), item, &record.content_version)?
        {
            return Ok(Promotion::VersionDeparted {
                category,
                disposal: None,
            });
        }

        // Content-addressed, idempotent: identical bytes recorded twice keep
        // one blob row and its first-seen time.
        tx.record_blob(account, hash, size, now_ms)?;

        // Per-attachment provenance (SYNC-052): the attachment keeps its own
        // identity, name, and version; only the verified bytes are shared.
        // Skip a link the projection cannot back rather than stranding the
        // materialization on a bookkeeping gap.
        let attachment_linked = match attachment_key(&canonical) {
            Some(key) if tx.read().attachment(&key)?.is_some() => {
                tx.link_attachment_blob(&key, hash, now_ms)?;
                true
            }
            _ => false,
        };

        // Fold any pin onto the row so a pinned item is never momentarily
        // evictable between promotion and the pin fold (POL-2, SYNC-051).
        let pin = tx.read().pin(item)?.map(|record| record.origin);

        tx.upsert_cache_entry(&CacheEntryRecord {
            item: item.clone(),
            account,
            content_version: record.content_version.clone(),
            kind: cache_kind(&canonical),
            size,
            blob_hash: Some(*hash),
            verification: CacheVerification::Verified,
            pin,
            last_access_at_ms: now_ms,
            materialized_at_ms: now_ms,
            materialization_ref: Some(materialization.reference.clone()),
        })?;
        tx.commit()?;

        Ok(Promotion::Materialized {
            hash: *hash,
            size,
            materialization_ref: materialization.reference,
            deduplicated: materialization.deduplicated,
            attachment_linked,
        })
    }

    /// Reads `[0, extent)` from the staged object and returns its SHA-256
    /// digest, or an integrity message if any expected byte cannot be read.
    /// `handle` is `None` only for the zero-byte object, whose digest is the
    /// empty-content hash and which reads nothing.
    fn hash_staged(
        &self,
        staging_host: &mut dyn StagingHost,
        transfer: TransferId,
        handle: Option<&str>,
        extent: u64,
    ) -> Result<ContentHash, String> {
        let mut hasher = Sha256::new();
        if extent == 0 {
            return Ok(hasher.content_hash());
        }
        let handle = handle.ok_or_else(|| {
            format!("transfer claims {extent} bytes of content but no staging area")
        })?;
        let staging = staging_host
            .open(transfer, Some(handle))
            .map_err(|error| format!("cannot open staging for verification: {error}"))?;

        let grain = self.config.read_chunk_bytes.get();
        let mut offset = 0u64;
        while offset < extent {
            let take = grain.min(extent - offset);
            let len = usize::try_from(take).map_err(|_| {
                format!("verification read of {take} bytes exceeds the address space")
            })?;
            let mut buffer = vec![0u8; len];
            staging
                .read_at(offset, &mut buffer)
                .map_err(|error| integrity_detail(&error, offset))?;
            hasher.update(&buffer);
            offset += take;
        }
        Ok(hasher.content_hash())
    }
}

/// Diagnostic for a staged-content read that failed during verification. Any
/// read error means the bytes cannot be trusted (the staging contract classes
/// a read past written bytes as `Failed`), so verification fails closed.
fn integrity_detail(error: &StagingError, offset: u64) -> String {
    format!("staged content unreadable at offset {offset}: {error}")
}

/// The staging area a not-yet-promoted transfer still holds, as a disposal
/// duty for the host.
fn staging_disposal(record: &TransferRecord) -> Option<StagingDisposal> {
    record
        .temp_ref
        .clone()
        .map(|staging| StagingDisposal { staging })
}

/// The canonical key an item resolves to, unwrapping a virtual appearance to
/// the canonical item it presents.
fn canonical_key(item: &ItemId) -> CanonicalKey {
    match item.key() {
        ItemKey::Canonical(canonical) => canonical,
        ItemKey::Appearance(appearance) => appearance.item,
    }
}

/// The account every canonical key is scoped to — blobs are account-scoped
/// content identity (DOM-021), so promotion always needs it.
fn canonical_account(key: &CanonicalKey) -> AccountKey {
    match key {
        CanonicalKey::Account(account) => *account,
        CanonicalKey::ChatList(key) => key.scope.account,
        CanonicalKey::FolderCatalog(key) => key.scope.account,
        CanonicalKey::Chat(key) => key.scope.account,
        CanonicalKey::YearDir(key) => key.chat.scope.account,
        CanonicalKey::MediaDir(key) => key.chat.scope.account,
        CanonicalKey::Message(key) => key.chat.scope.account,
        CanonicalKey::Attachment(key) => key.message.chat.scope.account,
        CanonicalKey::GeneratedDoc(key) => key.chat.scope.account,
        CanonicalKey::OrderDoc(key) => key.list.scope.account,
        CanonicalKey::Blob(key) => key.account,
    }
}

/// The attachment key an item names, if it is an attachment — the provenance
/// link target. Only attachments carry one.
fn attachment_key(key: &CanonicalKey) -> Option<AttachmentKey> {
    match key {
        CanonicalKey::Attachment(key) => Some(*key),
        _ => None,
    }
}

/// The cache accounting category an item materializes under (SYNC-050).
fn cache_kind(key: &CanonicalKey) -> CacheKind {
    match key {
        CanonicalKey::GeneratedDoc(_) | CanonicalKey::OrderDoc(_) => CacheKind::GeneratedDoc,
        _ => CacheKind::Blob,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use gramdrive_model::identity::{
        AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, ChatId, ChatKey,
        ChatListKind, GeneratedDocKey, ItemKey, MessageId, MessageKey, NamespaceVersion,
        SchemaFamily,
    };

    fn account() -> AccountKey {
        AccountKey {
            account_id: AccountId(7),
        }
    }

    fn scope() -> AccountScope {
        AccountScope {
            account: account(),
            namespace_version: NamespaceVersion(1),
        }
    }

    fn attachment(message: i64, index: u32) -> AttachmentKey {
        AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(message),
            },
            index: AttachmentIndex(index),
        }
    }

    fn item(key: impl Into<ItemKey>) -> ItemId {
        key.into().id()
    }

    #[test]
    fn default_config_reads_in_256_kib_grains() {
        assert_eq!(
            PromotionConfig::default().read_chunk_bytes.get(),
            256 * 1024
        );
    }

    #[test]
    fn account_and_attachment_derive_from_the_item_key() {
        let id = item(CanonicalKey::Attachment(attachment(5, 0)));
        let canonical = canonical_key(&id);
        assert_eq!(canonical_account(&canonical), account());
        assert_eq!(attachment_key(&canonical), Some(attachment(5, 0)));
        assert_eq!(cache_kind(&canonical), CacheKind::Blob);
    }

    #[test]
    fn appearance_unwraps_to_its_canonical_item() {
        // A blob is content, not presentation: an appearance resolves to the
        // canonical item it presents so account and provenance still derive.
        let canonical = CanonicalKey::Attachment(attachment(9, 2));
        let id = item(AppearanceKey {
            view: ChatListKind::Archive,
            item: canonical,
        });
        assert_eq!(attachment_key(&canonical_key(&id)), Some(attachment(9, 2)));
        assert_eq!(canonical_account(&canonical_key(&id)), account());
    }

    #[test]
    fn generated_documents_are_not_attachments_and_account_by_category() {
        let doc = CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            partition: gramdrive_model::identity::DocPartition::Chat,
            format: gramdrive_model::identity::DocFormat::Ndjson,
            schema_family: SchemaFamily(1),
        });
        let id = item(doc);
        assert_eq!(attachment_key(&canonical_key(&id)), None);
        assert_eq!(cache_kind(&canonical_key(&id)), CacheKind::GeneratedDoc);
        assert_eq!(canonical_account(&canonical_key(&id)), account());
    }
}
