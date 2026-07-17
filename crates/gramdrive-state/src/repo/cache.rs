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

use gramdrive_model::identity::{AccountId, AccountKey, ContentHash, ItemId};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    ReadTxn, WriteTxn, hash_columns, hash_from_columns, item_id_from_column, size_from_column,
    size_to_column,
};

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

    /// The eviction scan (POL-2): eligible rows only — unpinned, verified —
    /// oldest access first, via the partial index that contains nothing
    /// else (SYNC-051/052).
    pub fn eviction_candidates(&self, limit: u32) -> Result<Vec<EvictionCandidate>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id, size, last_access_at_ms FROM cache_entries
             WHERE pinned = 0 AND verification = 'verified'
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
            "SELECT kind, sum(size) FROM cache_entries WHERE account_id = ?1 GROUP BY kind",
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
    /// verified, checked in the delete itself (SYNC-051/052). Returns
    /// whether a row was removed; `false` means the entry was missing,
    /// pinned, or not verified, and the caller re-reads rather than
    /// assumes.
    pub fn evict_cache_entry(&self, item: &ItemId) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "DELETE FROM cache_entries
                 WHERE item_id = ?1 AND pinned = 0 AND verification = 'verified'",
            )?
            .execute(params![item.as_bytes()])?;
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
}
