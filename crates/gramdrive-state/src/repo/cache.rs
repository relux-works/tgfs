//! Materialized cache state and durable pin intent (POL-2,
//! SYNC-050..052).
//!
//! Two tables, two lifetimes: `cache_entries` describes bytes that exist on
//! disk right now; `pins` is offline intent that exists before hydration
//! and survives eviction of everything else. The engine folds intent onto
//! the materialized row ([`CacheEntryRecord::pin`]) so the eviction scan
//! needs no join — and eviction eligibility is enforced *in the delete
//! statement itself*: [`WriteTxn::evict_cache_entry`] cannot remove pinned
//! or unverified content no matter what the caller believes (SYNC-051/052).

use std::collections::HashSet;
use std::time::Duration;

use gramdrive_model::identity::{
    AccountId, AccountKey, CanonicalKey, ContentHash, ItemId, ItemKey,
};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params, params_from_iter};

use crate::error::StateError;
use crate::repo::{
    ReadTxn, WriteTxn, hash_columns, hash_from_columns, item_id_from_column, size_from_column,
    size_to_column,
};
use crate::store::{BUSY_TIMEOUT, StateStore};

/// SYNC-050 accounting category of a cache entry (`cache_entries.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// A materialized attachment blob.
    Blob,
    /// A materialized generated document.
    GeneratedDoc,
    /// A thumbnail.
    Thumbnail,
}

impl CacheKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::GeneratedDoc => "generated_doc",
            Self::Thumbnail => "thumbnail",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "blob" => Ok(Self::Blob),
            "generated_doc" => Ok(Self::GeneratedDoc),
            "thumbnail" => Ok(Self::Thumbnail),
            other => Err(StateError::CorruptRow {
                table: "cache_entries",
                detail: format!("unknown kind '{other}'"),
            }),
        }
    }
}

/// Verification state of materialized bytes (SYNC-052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheVerification {
    /// Not yet hashed; ineligible for eviction.
    Unverified,
    /// Hash-verified; the only eviction-eligible state.
    Verified,
    /// Verification failed; awaiting repair, never evicted as space.
    Corrupt,
}

impl CacheVerification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Corrupt => "corrupt",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "unverified" => Ok(Self::Unverified),
            "verified" => Ok(Self::Verified),
            "corrupt" => Ok(Self::Corrupt),
            other => Err(StateError::CorruptRow {
                table: "cache_entries",
                detail: format!("unknown verification '{other}'"),
            }),
        }
    }
}

/// Where a pin came from (POL-2): user intent and Archive-Mode coverage
/// release independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOrigin {
    /// An explicit user pin.
    User,
    /// Archive-Mode coverage.
    ArchiveMode,
}

impl PinOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ArchiveMode => "archive_mode",
        }
    }

    fn parse(table: &'static str, text: &str) -> Result<Self, StateError> {
        match text {
            "user" => Ok(Self::User),
            "archive_mode" => Ok(Self::ArchiveMode),
            other => Err(StateError::CorruptRow {
                table,
                detail: format!("unknown pin origin '{other}'"),
            }),
        }
    }
}

/// One materialized cache entry (domain-model § Cache entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntryRecord {
    /// The provider item the bytes materialize.
    pub item: ItemId,
    /// Owning account, for quota accounting (SYNC-050).
    pub account: AccountKey,
    /// The content version the bytes are valid for (SYNC-042).
    pub content_version: ContentVersion,
    /// Accounting category.
    pub kind: CacheKind,
    /// Size on disk in bytes.
    pub size: u64,
    /// Hash of the backing blob, when the entry materializes one.
    pub blob_hash: Option<ContentHash>,
    /// Verification state; gates eviction (SYNC-052).
    pub verification: CacheVerification,
    /// Pin intent folded onto the materialized row; `None` means evictable
    /// by policy.
    pub pin: Option<PinOrigin>,
    /// Last access, for LRU (ms since the Unix epoch).
    pub last_access_at_ms: i64,
    /// When the bytes were materialized (ms since the Unix epoch).
    pub materialized_at_ms: i64,
    /// The platform's opaque handle to the on-disk form.
    pub materialization_ref: Option<String>,
}

/// One row of the eviction scan (SYNC-051/052).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionCandidate {
    /// The evictable item.
    pub item: ItemId,
    /// Bytes that eviction would reclaim.
    pub size: u64,
    /// Last access, oldest first in the scan.
    pub last_access_at_ms: i64,
}

/// Cache usage of one accounting category (SYNC-050).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUsage {
    /// The category.
    pub kind: CacheKind,
    /// Total bytes materialized under it.
    pub total_bytes: u64,
}

/// Device-wide materialized-cache totals, split by the facts the quota
/// engine acts on (POL-2, SYNC-050/054). Every field sums `cache_entries`
/// across every account, because the on-disk cache is one device budget even
/// though blob identity is account-scoped (DOM-021).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheTotals {
    /// Every materialized byte, pinned or not.
    pub total_bytes: u64,
    /// Bytes an explicit pin or Archive-Mode coverage holds: quota-exempt,
    /// but still counted and surfaced (POL-2).
    pub pinned_bytes: u64,
    /// Quota-managed bytes with no pin. Generated documents are excluded
    /// because their category is independently quota-exempt, so this is not
    /// the complement of [`Self::pinned_bytes`].
    pub unpinned_bytes: u64,
    /// Bytes eviction can reclaim right now: unpinned *and* verified
    /// (SYNC-052). A subset of `unpinned_bytes`; the rest awaits hashing or
    /// repair and is never dropped as space.
    pub evictable_bytes: u64,
}

/// One durable pin (POL-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRecord {
    /// The pinned provider item.
    pub item: ItemId,
    /// Where the pin came from.
    pub origin: PinOrigin,
    /// When the pin was created (ms since the Unix epoch).
    pub created_at_ms: i64,
}

/// Truthful Archive-Mode eager hydration progress for one account.
///
/// `failed_allowed_items` is a subset of `pending_allowed_items`: a failed
/// current-generation transfer still needs backfill until verified bytes are
/// materialized. The category is the most recently updated terminal failure,
/// which gives hosts one deterministic actionable reason without hiding the
/// aggregate failure count.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArchiveBackfillProgressRecord {
    /// Allowed persistent items that still lack verified current bytes.
    pub pending_allowed_items: u64,
    /// Pending items whose current-generation transfer ended in failure.
    pub failed_allowed_items: u64,
    /// Stable category of the most recently updated failed pending item.
    pub failure_category: Option<String>,
}

/// One physical cache object awaiting crash-resumable deletion after a
/// destructive retention transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPurgeRecord {
    /// Account whose policy transition queued the object.
    pub account: AccountKey,
    /// Opaque host-owned cache handle.
    pub materialization_ref: String,
    /// Time the database transaction released its cache ownership.
    pub queued_at_ms: i64,
}

struct RawCacheEntry {
    item_id: Vec<u8>,
    account_id: i64,
    content_version: String,
    kind: String,
    size: i64,
    blob_hash_algo: Option<String>,
    blob_hash: Option<Vec<u8>>,
    verification: String,
    pinned: bool,
    pin_origin: Option<String>,
    last_access_at_ms: i64,
    materialized_at_ms: i64,
    materialization_ref: Option<String>,
}

fn read_cache_entry(row: &Row<'_>) -> Result<RawCacheEntry, rusqlite::Error> {
    Ok(RawCacheEntry {
        item_id: row.get(0)?,
        account_id: row.get(1)?,
        content_version: row.get(2)?,
        kind: row.get(3)?,
        size: row.get(4)?,
        blob_hash_algo: row.get(5)?,
        blob_hash: row.get(6)?,
        verification: row.get(7)?,
        pinned: row.get(8)?,
        pin_origin: row.get(9)?,
        last_access_at_ms: row.get(10)?,
        materialized_at_ms: row.get(11)?,
        materialization_ref: row.get(12)?,
    })
}

fn finish_cache_entry(raw: RawCacheEntry) -> Result<CacheEntryRecord, StateError> {
    let pin = match (raw.pinned, raw.pin_origin) {
        (false, None) => None,
        (true, Some(origin)) => Some(PinOrigin::parse("cache_entries", &origin)?),
        _ => {
            return Err(StateError::CorruptRow {
                table: "cache_entries",
                detail: "pinned flag and pin_origin must be set together".to_owned(),
            });
        }
    };
    Ok(CacheEntryRecord {
        item: item_id_from_column("cache_entries", &raw.item_id)?,
        account: AccountKey {
            account_id: AccountId(raw.account_id),
        },
        content_version: ContentVersion::new(raw.content_version).map_err(|error| {
            StateError::CorruptRow {
                table: "cache_entries",
                detail: format!("content_version does not parse: {error}"),
            }
        })?,
        kind: CacheKind::parse(&raw.kind)?,
        size: size_from_column("cache_entries", raw.size)?,
        blob_hash: hash_from_columns("cache_entries", raw.blob_hash_algo, raw.blob_hash)?,
        verification: CacheVerification::parse(&raw.verification)?,
        pin,
        last_access_at_ms: raw.last_access_at_ms,
        materialized_at_ms: raw.materialized_at_ms,
        materialization_ref: raw.materialization_ref,
    })
}

const CACHE_COLUMNS: &str = "item_id, account_id, content_version, kind, size,
     blob_hash_algo, blob_hash, verification, pinned, pin_origin,
     last_access_at_ms, materialized_at_ms, materialization_ref";

impl ReadTxn<'_> {
    /// One cache entry by item.
    pub fn cache_entry(&self, item: &ItemId) -> Result<Option<CacheEntryRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {CACHE_COLUMNS} FROM cache_entries WHERE item_id = ?1"
            ))?
            .query_row(params![item.as_bytes()], read_cache_entry)
            .optional()?;
        raw.map(finish_cache_entry).transpose()
    }

    /// Whether any current cache row still owns one materialization handle.
    ///
    /// Generated renderers use this after atomically replacing every live
    /// appearance: only an unclaimed immutable generation may be removed.
    pub fn cache_reference_claimed(&self, reference: &str) -> Result<bool, StateError> {
        self.conn()
            .prepare_cached(
                "SELECT EXISTS (
                 SELECT 1 FROM cache_entries WHERE materialization_ref = ?1)",
            )?
            .query_row(params![reference], |row| row.get(0))
            .map_err(StateError::from)
    }

    /// The subset of a bounded exact-reference batch currently claimed by
    /// cache rows.
    ///
    /// Generated reclamation snapshots its filesystem candidates first, then
    /// resolves the complete candidate set in one indexed query. Keeping this
    /// API exact (rather than prefix-scanning the account cache) preserves the
    /// `cache_entries_by_materialization_ref` query-plan bound.
    pub fn cache_references_claimed(
        &self,
        references: &[String],
    ) -> Result<HashSet<String>, StateError> {
        if references.is_empty() {
            return Ok(HashSet::new());
        }
        let placeholders = std::iter::repeat_n("?", references.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT DISTINCT materialization_ref FROM cache_entries \
             WHERE materialization_ref IN ({placeholders})"
        );
        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(references), |row| row.get(0))?;
        rows.collect::<Result<HashSet<String>, _>>()
            .map_err(StateError::from)
    }

    /// A verified materialization of one canonical blob/version, regardless
    /// of which appearance originally requested it.
    ///
    /// Story transitions replace the active appearance identity with a month
    /// appearance while retaining one canonical blob. This lookup lets the
    /// new appearance reuse that exact materialization without a duplicate
    /// cache row or byte object.
    pub fn verified_cache_entry_for_blob(
        &self,
        account: AccountKey,
        hash: &ContentHash,
        version: &ContentVersion,
        size: u64,
    ) -> Result<Option<CacheEntryRecord>, StateError> {
        let (algorithm, bytes) = hash_columns(hash);
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {CACHE_COLUMNS} FROM cache_entries
                 WHERE account_id = ?1 AND blob_hash_algo = ?2 AND blob_hash = ?3
                   AND content_version = ?4 AND size = ?5
                   AND verification = 'verified' AND materialization_ref IS NOT NULL
                 ORDER BY item_id LIMIT 1"
            ))?
            .query_row(
                params![
                    account.account_id.0,
                    algorithm,
                    bytes,
                    version.as_str(),
                    size_to_column(size)?,
                ],
                read_cache_entry,
            )
            .optional()?;
        raw.map(finish_cache_entry).transpose()
    }

    /// Every materialization owned by one account, in stable item order.
    ///
    /// Content-specific retention cleanup uses this bounded account scope and
    /// applies its own canonical policy before removing a row or its possibly
    /// shared object.
    pub fn cache_entries_for_account(
        &self,
        account: AccountKey,
    ) -> Result<Vec<CacheEntryRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id FROM cache_entries
             WHERE account_id = ?1 ORDER BY item_id",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let item = item_id_from_column("cache_entries", &row?)?;
            if let Some(entry) = self.cache_entry(&item)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// The eviction scan (POL-2): eligible rows only — unpinned, verified,
    /// and not generated documents — oldest access first. Generated documents
    /// are quota-exempt because their deterministic renderer may republish an
    /// immutable reference while the filesystem eviction is reclaiming it;
    /// their distinct accounting category keeps that exemption visible.
    pub fn eviction_candidates(&self, limit: u32) -> Result<Vec<EvictionCandidate>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id, size, last_access_at_ms FROM cache_entries
             WHERE pinned = 0 AND verification = 'verified'
               AND kind != 'generated_doc'
             ORDER BY last_access_at_ms LIMIT ?1",
        )?;
        let rows = statement.query_map(params![i64::from(limit)], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            let (item, size, last_access_at_ms) = row?;
            candidates.push(EvictionCandidate {
                item: item_id_from_column("cache_entries", &item)?,
                size: size_from_column("cache_entries", size)?,
                last_access_at_ms,
            });
        }
        Ok(candidates)
    }

    /// Cache usage of one account by category, from the covering
    /// accounting index (SYNC-050).
    pub fn cache_usage(&self, account: AccountKey) -> Result<Vec<CacheUsage>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT kind, sum(size) FROM (
                 SELECT kind, size FROM cache_entries WHERE account_id = ?1
                 UNION ALL
                 SELECT 'blob' AS kind, materialized_size AS size
                 FROM retained_attachment_versions
                 WHERE account_id = ?1 AND materialized_size IS NOT NULL
             )
             GROUP BY kind",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut usage = Vec::new();
        for row in rows {
            let (kind, total) = row?;
            usage.push(CacheUsage {
                kind: CacheKind::parse(&kind)?,
                total_bytes: size_from_column("cache_entries", total)?,
            });
        }
        Ok(usage)
    }

    /// Device-wide cache usage by category, summed across every account
    /// (SYNC-050). The on-disk cache is one device budget, so the quota
    /// engine needs the global figure the per-account [`ReadTxn::cache_usage`]
    /// does not give.
    pub fn cache_usage_by_kind(&self) -> Result<Vec<CacheUsage>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT kind, sum(size) FROM (
                 SELECT kind, size FROM cache_entries
                 UNION ALL
                 SELECT 'blob' AS kind, materialized_size AS size
                 FROM retained_attachment_versions
                 WHERE materialized_size IS NOT NULL
             )
             GROUP BY kind",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut usage = Vec::new();
        for row in rows {
            let (kind, total) = row?;
            usage.push(CacheUsage {
                kind: CacheKind::parse(&kind)?,
                total_bytes: size_from_column("cache_entries", total)?,
            });
        }
        Ok(usage)
    }

    /// Device-wide materialized-cache totals split by pin and verification
    /// (POL-2, SYNC-050/054) — the numbers a quota decision is made from in
    /// one pass over `cache_entries`. Generated documents are deliberately
    /// omitted from the quota-governed unpinned/evictable totals while still
    /// included in `total_bytes` and the by-kind usage breakdown. `CASE`-form
    /// sums keep the plan portable to any SQLite the toolchain pins.
    pub fn cache_totals(&self) -> Result<CacheTotals, StateError> {
        let (total, pinned, unpinned, evictable): (i64, i64, i64, i64) = self
            .conn()
            .prepare_cached(
                "WITH live AS (
                     SELECT
                         coalesce(sum(size), 0) AS total,
                         coalesce(sum(CASE WHEN pinned = 1 THEN size ELSE 0 END), 0)
                             AS pinned,
                         coalesce(sum(
                             CASE WHEN pinned = 0 AND kind != 'generated_doc'
                                  THEN size ELSE 0 END
                         ), 0)
                             AS unpinned,
                         coalesce(sum(
                             CASE WHEN pinned = 0 AND verification = 'verified'
                                       AND kind != 'generated_doc'
                                  THEN size ELSE 0 END
                         ), 0) AS evictable
                     FROM cache_entries
                 ),
                 audit AS (
                     SELECT coalesce(sum(materialized_size), 0) AS retained
                     FROM retained_attachment_versions
                     WHERE materialized_size IS NOT NULL
                 )
                 SELECT live.total + audit.retained,
                        live.pinned + audit.retained,
                        live.unpinned,
                        live.evictable
                 FROM live, audit",
            )?
            .query_row([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
        Ok(CacheTotals {
            total_bytes: size_from_column("cache_entries", total)?,
            pinned_bytes: size_from_column("cache_entries", pinned)?,
            unpinned_bytes: size_from_column("cache_entries", unpinned)?,
            evictable_bytes: size_from_column("cache_entries", evictable)?,
        })
    }

    /// One page of the eviction scan past a keyset cursor (POL-2,
    /// SYNC-051/052): eligible rows only — unpinned, verified, non-generated
    /// documents — in `(last_access_at_ms, item_id)` order, so the quota engine
    /// can walk the LRU frontier in bounded memory instead of loading the
    /// whole working set. `after` is an exclusive lower bound; `None` starts
    /// at the oldest. The tuple tiebreak on `item_id` makes pagination stable
    /// when several entries share a last-access timestamp.
    pub fn eviction_candidates_after(
        &self,
        after: Option<(i64, &ItemId)>,
        limit: u32,
    ) -> Result<Vec<EvictionCandidate>, StateError> {
        let (has_cursor, cursor_access, cursor_item) = match after {
            Some((access, item)) => (true, access, item.as_bytes().to_vec()),
            None => (false, 0, Vec::new()),
        };
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id, size, last_access_at_ms FROM cache_entries
             WHERE pinned = 0 AND verification = 'verified'
               AND kind != 'generated_doc'
               AND (?1 = 0
                    OR last_access_at_ms > ?2
                    OR (last_access_at_ms = ?2 AND item_id > ?3))
             ORDER BY last_access_at_ms, item_id
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![has_cursor, cursor_access, cursor_item, i64::from(limit)],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut candidates = Vec::new();
        for row in rows {
            let (item, size, last_access_at_ms) = row?;
            candidates.push(EvictionCandidate {
                item: item_id_from_column("cache_entries", &item)?,
                size: size_from_column("cache_entries", size)?,
                last_access_at_ms,
            });
        }
        Ok(candidates)
    }

    /// Whether any cache entry still names this on-disk object by its
    /// `materialization_ref` (SYNC-052 dedup). Content-addressed promotion
    /// lets several entries — even across accounts — share one object, so
    /// eviction must never delete the bytes while another entry still needs
    /// them. Checked *after* a row is deleted to decide if its object became
    /// an orphan the eviction may reclaim.
    pub fn materialization_ref_referenced(&self, reference: &str) -> Result<bool, StateError> {
        let found: Option<i64> = self
            .conn()
            .prepare_cached(
                "SELECT 1 FROM cache_entries WHERE materialization_ref = ?1
                 UNION ALL
                 SELECT 1 FROM retained_attachment_versions
                 WHERE materialization_ref = ?1
                 LIMIT 1",
            )?
            .query_row(params![reference], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// One pin by item.
    pub fn pin(&self, item: &ItemId) -> Result<Option<PinRecord>, StateError> {
        let raw: Option<(String, i64)> = self
            .conn()
            .prepare_cached("SELECT origin, created_at_ms FROM pins WHERE item_id = ?1")?
            .query_row(params![item.as_bytes()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        raw.map(|(origin, created_at_ms)| {
            Ok(PinRecord {
                item: item.clone(),
                origin: PinOrigin::parse("pins", &origin)?,
                created_at_ms,
            })
        })
        .transpose()
    }

    /// Every durable pin, optionally of one origin — Archive-Mode teardown
    /// releases exactly its own (POL-2).
    pub fn pins(&self, origin: Option<PinOrigin>) -> Result<Vec<PinRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id, origin, created_at_ms FROM pins
             WHERE ?1 IS NULL OR origin = ?1
             ORDER BY created_at_ms, item_id",
        )?;
        let rows = statement.query_map(params![origin.map(PinOrigin::as_str)], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut pins = Vec::new();
        for row in rows {
            let (item, origin, created_at_ms) = row?;
            pins.push(PinRecord {
                item: item_id_from_column("pins", &item)?,
                origin: PinOrigin::parse("pins", &origin)?,
                created_at_ms,
            });
        }
        Ok(pins)
    }

    /// Bounded Archive-Mode hydration worklist. Only live, fetchable items
    /// with Archive-Mode pin ownership and no verified materialization are
    /// returned; Audit mode is deliberately not consulted.
    pub fn archive_backfill_candidates(
        &self,
        account: AccountKey,
        limit: u32,
    ) -> Result<Vec<ItemId>, StateError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut statement = self.conn().prepare_cached(
            "SELECT p.item_id FROM pins p
             JOIN items i ON i.item_id = p.item_id
             LEFT JOIN cache_entries c ON c.item_id = p.item_id
             WHERE i.account_id = ?1 AND p.origin = 'archive_mode'
               AND i.deleted_at_ms IS NULL AND i.availability = 'fetchable'
               AND i.content_version IS NOT NULL
               AND (c.item_id IS NULL OR c.content_version <> i.content_version
                    OR c.verification <> 'verified'
                    OR c.materialization_ref IS NULL)
            ORDER BY p.created_at_ms, p.item_id",
        )?;
        let rows = statement.query_map(params![account.account_id.0], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut items = Vec::new();
        for row in rows {
            let item = item_id_from_column("pins", &row?)?;
            let attachment = match item.key() {
                ItemKey::Canonical(CanonicalKey::Attachment(key))
                | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
                    item: CanonicalKey::Attachment(key),
                    ..
                }) => Some(key),
                _ => None,
            };
            if let Some(key) = attachment {
                // Legacy/projected-only fixtures may not have normalized
                // attachment facts. When facts do exist they are the
                // authoritative source-policy gate and must override a stale
                // fetchable item row.
                let source_allowed = self.attachment(&key)?.is_none_or(|attachment| {
                    attachment.facts.can_be_saved
                        && attachment.facts.availability
                            == crate::repo::AttachmentAvailability::Fetchable
                });
                let chat_allowed = self
                    .chat(&key.message.chat)?
                    .is_none_or(|chat| !chat.is_protected);
                let allowed = source_allowed && chat_allowed;
                if !allowed {
                    continue;
                }
            }
            items.push(item);
            if items.len() >= limit {
                break;
            }
        }
        Ok(items)
    }

    /// Counts the same allowed persistent candidates as
    /// [`Self::archive_backfill_candidates`] and projects durable terminal
    /// transfer failures for their current content generations.
    pub fn archive_backfill_progress(
        &self,
        account: AccountKey,
    ) -> Result<ArchiveBackfillProgressRecord, StateError> {
        let (pending, failed, failure_category): (i64, i64, Option<String>) = self
            .conn()
            .prepare_cached(
                "WITH candidates AS (
                     SELECT p.item_id, i.content_version
                     FROM pins p
                     JOIN items i ON i.item_id = p.item_id
                     LEFT JOIN cache_entries c ON c.item_id = p.item_id
                     WHERE i.account_id = ?1 AND p.origin = 'archive_mode'
                       AND i.deleted_at_ms IS NULL AND i.availability = 'fetchable'
                       AND i.content_version IS NOT NULL
                       AND (c.item_id IS NULL OR c.content_version <> i.content_version
                            OR c.verification <> 'verified'
                            OR c.materialization_ref IS NULL)
                 )
                 SELECT COUNT(*),
                        COALESCE(SUM(
                            CASE WHEN EXISTS (
                                SELECT 1 FROM transfers t
                                WHERE t.item_id = candidates.item_id
                                  AND t.content_version = candidates.content_version
                                  AND t.state = 'failed'
                            ) THEN 1 ELSE 0 END
                        ), 0),
                        (
                            SELECT t.failure_category
                            FROM candidates latest
                            JOIN transfers t
                              ON t.item_id = latest.item_id
                             AND t.content_version = latest.content_version
                            WHERE t.state = 'failed'
                            ORDER BY t.updated_at_ms DESC, t.transfer_id DESC
                            LIMIT 1
                        )
                 FROM candidates",
            )?
            .query_row([account.account_id.0], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        Ok(ArchiveBackfillProgressRecord {
            pending_allowed_items: size_from_column("items", pending)?,
            failed_allowed_items: size_from_column("transfers", failed)?,
            failure_category,
        })
    }

    /// Pending physical-file deletions for one account in durable order.
    pub fn retention_purge_queue(
        &self,
        account: AccountKey,
        limit: u32,
    ) -> Result<Vec<RetentionPurgeRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT materialization_ref, queued_at_ms FROM retention_purge_queue
             WHERE account_id = ?1
             ORDER BY queued_at_ms, materialization_ref LIMIT ?2",
        )?;
        let rows = statement.query_map(params![account.account_id.0, i64::from(limit)], |row| {
            Ok(RetentionPurgeRecord {
                account,
                materialization_ref: row.get(0)?,
                queued_at_ms: row.get(1)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(StateError::from)
    }
}

impl StateStore {
    /// Attempts one LRU touch without waiting for a concurrent writer.
    ///
    /// A cache hit has already verified its durable bytes before this is
    /// called, so failing to refresh recency must never turn that hit into a
    /// hydration failure. The normal busy timeout is restored before this
    /// method returns; other write paths still retain their five-second
    /// serialization contract.
    ///
    /// Returns `Ok(false)` when another connection owns the writer or when
    /// the entry disappeared before the best-effort transaction acquired it.
    pub fn try_touch_cache_entry(
        &mut self,
        item: &ItemId,
        now_ms: i64,
    ) -> Result<bool, StateError> {
        self.connection().busy_timeout(Duration::ZERO)?;
        let outcome = (|| {
            let tx = self
                .connection_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let tx = WriteTxn::new(tx);
            let touched = tx.touch_cache_entry(item, now_ms)?;
            tx.commit()?;
            Ok(touched)
        })();
        let restore = self.connection().busy_timeout(BUSY_TIMEOUT);

        match (outcome, restore) {
            (Ok(touched), Ok(())) => Ok(touched),
            (Err(StateError::Sqlite(error)), Ok(()))
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                Ok(false)
            }
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error.into()),
        }
    }
}

impl WriteTxn<'_> {
    /// Inserts or fully replaces one cache entry. The item must already be
    /// projected; the blob, when referenced, already recorded.
    pub fn upsert_cache_entry(&self, record: &CacheEntryRecord) -> Result<(), StateError> {
        if record.materialization_ref.as_deref() == Some("") {
            return Err(StateError::InvalidArgument {
                what: "cache materialization_ref must not be empty text",
            });
        }
        let (algo, bytes) = match &record.blob_hash {
            Some(hash) => {
                let (algo, bytes) = hash_columns(hash);
                (Some(algo), Some(bytes))
            }
            None => (None, None),
        };
        self.conn()
            .prepare_cached(
                "INSERT INTO cache_entries (item_id, account_id, content_version, kind, size,
                                            blob_hash_algo, blob_hash, verification, pinned,
                                            pin_origin, last_access_at_ms, materialized_at_ms,
                                            materialization_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT (item_id) DO UPDATE SET
                     account_id = excluded.account_id,
                     content_version = excluded.content_version,
                     kind = excluded.kind,
                     size = excluded.size,
                     blob_hash_algo = excluded.blob_hash_algo,
                     blob_hash = excluded.blob_hash,
                     verification = excluded.verification,
                     pinned = excluded.pinned,
                     pin_origin = excluded.pin_origin,
                     last_access_at_ms = excluded.last_access_at_ms,
                     materialized_at_ms = excluded.materialized_at_ms,
                     materialization_ref = excluded.materialization_ref",
            )?
            .execute(params![
                record.item.as_bytes(),
                record.account.account_id.0,
                record.content_version.as_str(),
                record.kind.as_str(),
                size_to_column(record.size)?,
                algo,
                bytes,
                record.verification.as_str(),
                record.pin.is_some(),
                record.pin.map(PinOrigin::as_str),
                record.last_access_at_ms,
                record.materialized_at_ms,
                record.materialization_ref,
            ])?;
        Ok(())
    }

    /// Records an access for LRU purposes. Touching an unmaterialized item
    /// is a no-op — returns whether an entry was touched.
    pub fn touch_cache_entry(&self, item: &ItemId, now_ms: i64) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached("UPDATE cache_entries SET last_access_at_ms = ?2 WHERE item_id = ?1")?
            .execute(params![item.as_bytes(), now_ms])?;
        Ok(changed > 0)
    }

    /// Sets the verification state of a materialized entry (SYNC-052).
    pub fn set_cache_verification(
        &self,
        item: &ItemId,
        verification: CacheVerification,
    ) -> Result<(), StateError> {
        let changed = self
            .conn()
            .prepare_cached("UPDATE cache_entries SET verification = ?2 WHERE item_id = ?1")?
            .execute(params![item.as_bytes(), verification.as_str()])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "cache entry",
            });
        }
        Ok(())
    }

    /// Folds pin intent onto the materialized row so the eviction scan
    /// needs no join (POL-2). `None` makes the entry evictable by policy.
    pub fn set_cache_pin(&self, item: &ItemId, pin: Option<PinOrigin>) -> Result<(), StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE cache_entries SET pinned = ?2, pin_origin = ?3 WHERE item_id = ?1",
            )?
            .execute(params![
                item.as_bytes(),
                pin.is_some(),
                pin.map(PinOrigin::as_str)
            ])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "cache entry",
            });
        }
        Ok(())
    }

    /// Evicts one entry — but only if it is eligible: unpinned and
    /// verified, checked in the delete itself (SYNC-051/052). When the
    /// removed entry belongs to a generated document, its render state is
    /// marked dirty in this same transaction. This is what makes eviction a
    /// reversible depublishing operation rather than a state where item facts
    /// still advertise bytes that neither exist nor have a pending render.
    /// Returns whether a row was removed; `false` means the entry was missing,
    /// pinned, or not verified, and the caller re-reads rather than assumes.
    pub fn evict_cache_entry(&self, item: &ItemId) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "DELETE FROM cache_entries
                 WHERE item_id = ?1 AND pinned = 0 AND verification = 'verified'",
            )?
            .execute(params![item.as_bytes()])?;
        if changed > 0 && self.read().render_state(item)?.is_some() {
            // A render state exists only for generated documents. Keep the
            // cache-row removal and re-render scheduling atomic: a failed
            // scheduling write aborts the enclosing transaction, leaving the
            // byte ownership row in place.
            self.mark_render_dirty(item)?;
        }
        Ok(changed > 0)
    }

    /// Removes one entry unconditionally — account teardown and corrupt-
    /// entry repair, where POL-2 eligibility is not the question. Returns
    /// whether a row existed.
    pub fn remove_cache_entry(&self, item: &ItemId) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached("DELETE FROM cache_entries WHERE item_id = ?1")?
            .execute(params![item.as_bytes()])?;
        Ok(changed > 0)
    }

    /// Releases every byte-retention intent for one restricted item, removes
    /// its cache ownership, and journals the physical object for idempotent
    /// deletion after commit.
    ///
    /// The account predicate prevents a caller from crossing account
    /// ownership even if it supplies an item identity from another scope.
    /// A pin is removed even when no cache row exists, because Telegram
    /// restrictions override both Archive Mode and explicit offline intent.
    pub fn queue_restricted_cache_purge(
        &self,
        account: AccountKey,
        item: &ItemId,
        queued_at_ms: i64,
    ) -> Result<bool, StateError> {
        self.conn()
            .prepare_cached(
                "DELETE FROM pins
                 WHERE item_id = ?1
                   AND EXISTS (
                       SELECT 1 FROM items i
                       WHERE i.item_id = pins.item_id AND i.account_id = ?2
                   )",
            )?
            .execute(params![item.as_bytes(), account.account_id.0])?;

        let reference: Option<Option<String>> = self
            .conn()
            .prepare_cached(
                "SELECT materialization_ref FROM cache_entries
                 WHERE item_id = ?1 AND account_id = ?2",
            )?
            .query_row(params![item.as_bytes(), account.account_id.0], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(reference) = reference else {
            return Ok(false);
        };
        self.conn()
            .prepare_cached(
                "DELETE FROM cache_entries
                 WHERE item_id = ?1 AND account_id = ?2",
            )?
            .execute(params![item.as_bytes(), account.account_id.0])?;
        if let Some(reference) = reference {
            self.conn()
                .prepare_cached(
                    "INSERT INTO retention_purge_queue (
                         account_id, materialization_ref, queued_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (account_id, materialization_ref) DO NOTHING",
                )?
                .execute(params![account.account_id.0, reference, queued_at_ms])?;
        }
        Ok(true)
    }

    /// Records durable offline intent for an item (POL-2). Re-pinning
    /// updates the origin — a user pin over Archive-Mode coverage survives
    /// Archive Mode turning off — and keeps the original creation time.
    pub fn pin_item(
        &self,
        item: &ItemId,
        origin: PinOrigin,
        created_at_ms: i64,
    ) -> Result<(), StateError> {
        self.conn()
            .prepare_cached(
                "INSERT INTO pins (item_id, origin, created_at_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT (item_id) DO UPDATE SET origin = excluded.origin",
            )?
            .execute(params![item.as_bytes(), origin.as_str(), created_at_ms])?;
        Ok(())
    }

    /// Releases the pin on an item, if any. Returns whether a pin existed.
    /// The materialized row's folded flag is separate on purpose — release
    /// it with [`WriteTxn::set_cache_pin`] in the same transaction.
    pub fn unpin_item(&self, item: &ItemId) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached("DELETE FROM pins WHERE item_id = ?1")?
            .execute(params![item.as_bytes()])?;
        Ok(changed > 0)
    }

    /// Acknowledges one idempotently deleted retention-purge object.
    pub fn acknowledge_retention_purge(
        &self,
        account: AccountKey,
        materialization_ref: &str,
    ) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "DELETE FROM retention_purge_queue
                 WHERE account_id = ?1 AND materialization_ref = ?2",
            )?
            .execute(params![account.account_id.0, materialization_ref])?;
        Ok(changed > 0)
    }
}
