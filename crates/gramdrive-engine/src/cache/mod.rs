//! Integrity verification and atomic cache promotion (TASK-260715-3s6cpe;
//! SYNC-042, SYNC-050..053).
//!
//! # What this layer owns
//!
//! The [`transfer`](crate::transfer) machine proves what durable state alone
//! can — range coverage and the version pin — and ends a finished transfer at
//! [`CompleteOutcome::Promoted`](crate::transfer::CompleteOutcome::Promoted),
//! handing back the staging area holding the assembled bytes. This module is
//! the step that layers over that outcome: it *reads* those bytes, and only
//! bytes it can prove complete and correct ever become cache.
//!
//! One entry point, [`Promoter::promote`], does four things in order, and the
//! order is the crash-safety design:
//!
//! 1. **Verify (SYNC-042 integrity half).** Hash the whole staged object with
//!    SHA-256 ([`gramdrive_model::hash`]) and confirm every expected byte is
//!    readable. A staged object short of the item's extent, or one whose bytes
//!    can no longer be read, fails closed ([`Promotion::IntegrityFailed`]) —
//!    nothing is published, and the untrusted staging is handed back for the
//!    host to drop and the content to re-fetch on demand. The SHA-256 digest
//!    is the blob's content identity
//!    ([`ContentHash`](gramdrive_model::identity::ContentHash), DOM-021).
//! 2. **Re-check the pin (SYNC-042 version half).** If the item's current
//!    content version left the one the bytes were fetched for, publication is
//!    refused ([`Promotion::VersionDeparted`]) — bytes fetched for version A
//!    are never observable as version B.
//! 3. **Promote the file (file-before-row).** The host
//!    ([`PromotionHost::promote`]) atomically moves the verified staging object
//!    into content-addressed cache storage, keyed by the hash. This is durable
//!    before it returns (the host's fsync duty, documented on the trait), so
//!    once the object exists the only remaining work is the database row.
//! 4. **Publish (one transaction).** In a single write transaction the blob is
//!    recorded (content-addressed, idempotent), the cache entry is written
//!    `verified`, and — for an attachment — the attachment is linked to the
//!    blob, preserving its own identity and provenance
//!    ([`WriteTxn::link_attachment_blob`](gramdrive_state::WriteTxn::link_attachment_blob)).
//!
//! # Crash safety rests on reconciliation, not on a distributed transaction
//!
//! A file rename and a database commit cannot be one atomic act across a
//! crash, so the ordering above is chosen so that every interruption is a
//! *reconcilable* disagreement, never a corrupt one (SYNC-053, handled by
//! [`StateStore::reconcile`](gramdrive_state::StateStore::reconcile)):
//!
//! * Crash after the file promote, before the commit → a cache object no row
//!   claims: reconciliation's `OrphanCacheObject` deletes it, and the content
//!   re-fetches on demand. The database, the authority on what is cached,
//!   never named bytes it could not serve.
//! * Crash before the file promote → the staging object is intact but its
//!   transfer is terminal, so reconciliation's `LeakedStaging` reclaims it.
//!
//! Because the cache-object handle is a deterministic function of the content
//! hash, the promote is **idempotent**: promoting bytes whose object already
//! exists is a rename onto an existing name (a no-op the host reports as
//! `deduplicated`), which is exactly what makes **content-addressed dedup**
//! fall out for free — two attachments with identical bytes share one on-disk
//! object and one blob row while keeping two distinct attachment identities.
//! Re-invoking [`Promoter::promote`] after a committed promotion is likewise a
//! no-op ([`Promotion::AlreadyMaterialized`]): a verified cache entry for the
//! item at this version is proof the work is already done, so a consumed
//! staging object is never read a second time.
//!
//! # Quota, pinning, and eviction
//!
//! Promotion writes the cache; [`quota`] governs how much of it may live and
//! which bytes give way under pressure (TASK-260715-11abx8; POL-2,
//! SYNC-050..054). A partial-range transfer streamed its bytes to readers and
//! is not a blob (domain-model § Blob): promotion reports
//! [`Promotion::NotWholeContent`] and materializes nothing; those staged bytes
//! are accounted separately ([`CacheAccounting::partial_transfer_bytes`]) and
//! reclaimed by cancellation, never eviction. Promotion folds any existing pin
//! onto the row it writes so a pinned item is never momentarily evictable, and
//! surfaces a host storage refusal as
//! [`EngineError::Storage`](crate::transfer::EngineError::Storage); the
//! [`Evictor`] is what turns that signal into reclaimed space.
//!
//! * [`Evictor`] — device cache accounting ([`CacheAccounting`], SYNC-050),
//!   the actionable quota status ([`QuotaAssessment`], SYNC-054), and LRU
//!   eviction of eligible unpinned content ([`Evictor::enforce`],
//!   [`Evictor::reclaim`]; POL-2, SYNC-051/052). Eviction never races an open
//!   read or a live transfer, and deletes an on-disk object only once no
//!   surviving entry references it (dedup, SYNC-052); the object delete is
//!   row-before-file, so a crash leaves a reconcilable orphan (SYNC-053).
//! * [`pin`] / [`unpin`] — durable offline intent folded onto the materialized
//!   row, with directional origin so a user pin and Archive-Mode coverage do
//!   not clobber each other (POL-2, SYNC-051/062).
//!
//! System/provider eviction — bytes the OS purges out from under a live cache
//! row — is reconciled by
//! [`StateStore::reconcile`](gramdrive_state::StateStore::reconcile)
//! (SYNC-053, TASK-260715-3s6cpe/22fh09); accounting here reads the durable
//! rows and so assumes a reconciled store.

mod pin;
mod promote;
mod quota;

pub use pin::{PinOutcome, pin, unpin};
pub use promote::{
    Materialization, Promoter, Promotion, PromotionConfig, PromotionHost, PromotionHostError,
};
pub use quota::{
    CacheAccounting, DEFAULT_QUOTA_BYTES, EvictionPlan, EvictionReport, EvictionRequest, Evictor,
    QuotaAssessment, QuotaPolicy,
};
