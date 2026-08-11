//! The TDLib download adapter: the ranged-read (`DriveSource::fetch`) side
//! of the local tdjson source (TASK-260715-1onbmf; SYNC-040..046, POL-4,
//! DOM-007).
//!
//! # Where it sits
//!
//! The engine's ranged fetch coordinator (TASK-260715-22fh09) drives a
//! `DriveSource` with chunk-aligned [`FetchRequest`]s (SYNC-041) and owns
//! retry policy, coalescing, and staging. This module answers one such
//! request: it turns the pinned `(item, version, range)` into TDLib file
//! work and delivers exactly the requested bytes into the caller's
//! [`ContentSink`]. The full `DriveSource` implementation — enumeration,
//! cursors, thumbnails — composes later in its owning tasks; this module is
//! deliberately only the fetch side, shaped so that adapter delegates to it
//! unchanged ([`TdDownloader::fetch`] has `DriveSource::fetch`'s exact
//! signature).
//!
//! # Shape: a sans-IO machine and a thin driver
//!
//! [`DownloadMachine`] holds every decision — the pre-network gates, the
//! TDLib request protocol, response validation, failure classification,
//! delivery geometry — and performs no I/O, following the crate's machine
//! convention ([`crate::history`], [`crate::snapshot`]). [`TdDownloader`]
//! is the composing driver: it resolves the item through a [`FetchCatalog`],
//! submits the machine's requests on a [`TdClient`], reads local bytes, and
//! feeds the sink. Everything testable without a filesystem or a runtime is
//! in the machine.
//!
//! [`FetchCatalog`] is the seam to the metadata this crate does not own:
//! mapping an [`ItemId`] to its Telegram locators, POL-4 availability,
//! extent, and current content version is the state layer's projection, and
//! the composing adapter supplies it. This module only *consumes* those
//! facts — and re-checks them before every delivered chunk, so a
//! mid-fetch content change surfaces as a typed conflict rather than as
//! stale bytes (SYNC-042).
//!
//! # The protocol: synchronous ranged download, then local reads
//!
//! One fetch is `downloadFile {file_id, priority, offset, limit,
//! synchronous: true}` — TDLib resumes from whatever prefix its cache
//! already holds, so retries re-download nothing — followed by direct reads
//! of the reported `local.path` in [`DownloadConfig::read_chunk_bytes`]
//! slices, delivered to the sink in offset order. Nothing buffers more than
//! one slice (story AC: no whole-file memory buffering). The engine already
//! grids fetches to modest chunks (SYNC-041), so a synchronous per-range
//! download is the right grain, and delivery itself is the progress signal
//! the contract specifies (SYNC-046); [`DownloadMachine::progress`] exposes
//! the same accounting for observability (NFR-033). `updateFile` push
//! updates are deliberately not consumed: they carry nothing a synchronous
//! response does not, and the update stream belongs to the live-update
//! machinery.
//!
//! # Temporary-file ownership
//!
//! Every path TDLib reports lives in TDLib's own files directory and stays
//! TDLib's property: this module opens it read-only and never moves,
//! renames, truncates, or deletes it — deleting TDLib-cached content is a
//! TDLib request (`deleteFile`), owned by whoever owns cache policy, never
//! a filesystem operation. The handoff to the caller is bytes into the
//! sink, not a path; the engine stages them under its own transfer identity
//! (SYNC-042) and TDLib's cache stays internally consistent.
//!
//! # Version verification
//!
//! The fetch is pinned (DOM-003): the gate compares the catalog's current
//! content version against the pin before any request, and the driver
//! re-resolves before each delivered slice ([`DownloadMachine::observe_entry`]).
//! A TDLib `file_id` names immutable remote content — an edit that changes
//! bytes mints a new file — so bytes read under a verified locator cannot
//! be another version's; the re-checks close the model-level race where the
//! catalog moves on mid-fetch, failing with
//! [`SourceError::VersionConflict`] instead of completing (SYNC-042).
//!
//! # Reference refresh (SYNC-045, DOM-007)
//!
//! A `downloadFile` rejected with Telegram's `FILE_REFERENCE_*` class means
//! the remote locator went stale — refreshable metadata, never identity.
//! The machine then asks for the containing message (`getMessage`) or story
//! (`getStory`), which
//! makes TDLib re-learn the reference for the same `file_id`, verifies the
//! refreshed object still names the pinned content (a changed
//! `remote_unique_id` is a version conflict, not a refresh), and resolves
//! the call with [`SourceError::StaleReference`] — the category whose
//! contract is "the adapter refreshes and the caller retries". The retry
//! then succeeds against the refreshed reference, and no identity moved.
//!
//! # One download conversation per file
//!
//! TDLib keeps a single download position per file: a second `downloadFile`
//! with a different offset/limit displaces the first, and a displaced
//! synchronous request resolves early with its range not covered. Concurrent
//! fetches of one item therefore serialize on a per-file lock inside
//! [`TdDownloader`] — correctness under SYNC-046's concurrent-callers rule;
//! *coalescing* concurrent demand stays the engine's job. Fetches of
//! different files proceed independently.
//!
//! # Cancellation (SYNC-005, SYNC-043)
//!
//! Both contract paths are prompt. Dropping the fetch future at any await —
//! the download wait, or the yield after each delivered slice — abandons
//! the work, and a guard fires `cancelDownloadFile` so TDLib stops network
//! work rather than finishing a download nobody wants. A sink answering
//! [`SinkControl::Stop`] resolves the fetch with [`SourceError::Cancelled`];
//! by then the ranged download is already complete locally, so there is no
//! network work left to stop, and local reads simply cease.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use serde_json::{Value, json};

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{
    ContentChunk, ContentSink, ContentSource, FetchRequest, SinkControl, SourceError, SourceFuture,
};

use crate::error::{TdError, retryable_after};
use crate::message::{AttachmentAvailability, normalize_message};
use crate::runtime::TdClient;
use crate::story::normalize_story;

/// Default local read slice: 256 KiB. Bounds per-fetch memory to one slice
/// and sits below the engine's default chunk grid, so even a whole-chunk
/// fetch delivers in a few slices with a cancellation point between each.
const DEFAULT_READ_CHUNK_BYTES: u64 = 256 * 1024;

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// TDLib download priority: 1..=32, higher first, exactly the range
/// `downloadFile` accepts. The engine's scheduling priority maps onto this
/// at composition; this type only guarantees the passthrough stays inside
/// TDLib's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DownloadPriority(u8);

impl DownloadPriority {
    /// The lowest priority TDLib accepts.
    pub const MIN: DownloadPriority = DownloadPriority(1);
    /// The highest priority TDLib accepts.
    pub const MAX: DownloadPriority = DownloadPriority(32);

    /// Wraps a TDLib priority, rejecting values outside 1..=32.
    pub fn new(value: u8) -> Result<DownloadPriority, InvalidPriority> {
        if (Self::MIN.0..=Self::MAX.0).contains(&value) {
            Ok(DownloadPriority(value))
        } else {
            Err(InvalidPriority { value })
        }
    }

    /// The raw value passed to `downloadFile`.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for DownloadPriority {
    /// The midpoint, 16 — leaves headroom both ways for the composing
    /// caller's visible/background mapping.
    fn default() -> DownloadPriority {
        DownloadPriority(16)
    }
}

/// A priority outside TDLib's 1..=32 contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPriority {
    /// The rejected value.
    pub value: u8,
}

impl std::fmt::Display for InvalidPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "download priority {} is outside TDLib's 1..=32",
            self.value
        )
    }
}

impl std::error::Error for InvalidPriority {}

// ---------------------------------------------------------------------------
// The catalog seam
// ---------------------------------------------------------------------------

/// The per-item facts a fetch needs, resolved by the composing caller's
/// metadata projection (the state layer, in the full adapter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    /// TDLib's file id — the locator `downloadFile` takes, stable within
    /// one running TDLib client and naming immutable remote content.
    pub file_id: i32,
    /// Telegram's durable remote file locator. Unlike `file_id`, this
    /// survives client/database restarts and can be rebound to the current
    /// process-local id with `getRemoteFile`.
    pub remote_id: Option<String>,
    /// Exact TDLib file constructor paired with `getRemoteFile`. Resolving
    /// media as an unknown file can produce a handle that `downloadFile`
    /// cannot rematerialize.
    pub remote_file_type: Option<RemoteFileType>,
    /// Canonical owner used to refresh an expired Telegram file reference.
    pub refresh: RefreshTarget,
    /// POL-4 availability. Anything but
    /// [`AttachmentAvailability::Fetchable`] is rejected before any
    /// network call.
    pub availability: AttachmentAvailability,
    /// Telegram's stable content identifier, when known — what a refresh
    /// is verified against (a changed unique id is a version conflict,
    /// not a refresh).
    pub remote_unique_id: Option<String>,
    /// Extent in bytes, when the projection knows it. A known extent
    /// rejects an unsatisfiable range before any network call; an unknown
    /// one defers the check to the download response.
    pub size: Option<u64>,
    /// The content version the source currently serves for this item —
    /// compared against the fetch's pin (DOM-003, SYNC-042).
    pub version: ContentVersion,
}

/// TDLib `FileType` constructors used by persisted downloadable content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFileType {
    /// `messageDocument.document`.
    Document,
    /// `messagePhoto.photo`.
    Photo,
    /// `messageVideo.video`.
    Video,
    /// `messageAnimation.animation`.
    Animation,
    /// `messageAudio.audio`.
    Audio,
    /// `messageVoiceNote.voice`.
    VoiceNote,
    /// `messageVideoNote.video_note`.
    VideoNote,
    /// `messageSticker.sticker`.
    Sticker,
    /// A story photo primary locator.
    PhotoStory,
    /// A story video primary locator.
    VideoStory,
    /// A file-backed thumbnail locator.
    Thumbnail,
}

impl RemoteFileType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Document => "fileTypeDocument",
            Self::Photo => "fileTypePhoto",
            Self::Video => "fileTypeVideo",
            Self::Animation => "fileTypeAnimation",
            Self::Audio => "fileTypeAudio",
            Self::VoiceNote => "fileTypeVoiceNote",
            Self::VideoNote => "fileTypeVideoNote",
            Self::Sticker => "fileTypeSticker",
            Self::PhotoStory => "fileTypePhotoStory",
            Self::VideoStory => "fileTypeVideoStory",
            Self::Thumbnail => "fileTypeThumbnail",
        }
    }
}

/// Canonical source object that can refresh one TDLib file reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTarget {
    /// A normal message attachment refreshed through `getMessage`.
    Message {
        /// Chat containing the message.
        chat_id: i64,
        /// Telegram message identifier.
        message_id: i64,
    },
    /// Save-permitted story content refreshed through non-viewing `getStory`.
    Story {
        /// Chat that posted the story.
        poster_chat_id: i64,
        /// Telegram story identifier.
        story_id: i64,
    },
}

/// What an [`ItemId`] resolves to, for fetch purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogEntry {
    /// A directory: fetching it is [`SourceError::InvalidRequest`].
    Directory,
    /// A file and the facts a download needs.
    File(FileTarget),
}

/// Resolution of item identity to fetch facts — the seam between this
/// adapter and the metadata store it must not own. `None` means the item
/// does not exist ([`SourceError::NotFound`]).
///
/// Implementations answer from local state and must return promptly; they
/// are called before every delivered slice, which is what makes mid-fetch
/// version drift observable (SYNC-042).
pub trait FetchCatalog: Send + Sync {
    /// The entry for `item`, or `None` when no such item exists.
    fn resolve(&self, item: &ItemId) -> Option<CatalogEntry>;

    /// Persists locator facts learned by a successful source-object refresh.
    ///
    /// The default keeps in-memory/test catalogs source-compatible. Durable
    /// state catalogs override it so a relaunch does not forget the refreshed
    /// locator. Implementations must preserve item and content identity.
    fn persist_refresh(
        &self,
        _item: &ItemId,
        _refresh: &RefreshedFileTarget,
    ) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Locator facts verified from the source object returned by a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedFileTarget {
    /// TDLib's process-local numeric file locator.
    pub file_id: i32,
    /// Telegram's current remote file locator.
    pub remote_id: Option<String>,
    /// Telegram's stable content identity.
    pub remote_unique_id: Option<String>,
    /// Current exact extent, when TDLib reports one.
    pub size: Option<u64>,
    /// Current saveability gate.
    pub availability: AttachmentAvailability,
    /// Telegram's raw per-message save permission.
    pub can_be_saved: bool,
}

/// Download adapter tuning. Policy, not durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadConfig {
    /// TDLib priority passed through to every `downloadFile` (1..=32).
    pub priority: DownloadPriority,
    /// Local read slice size: bounds per-fetch memory and sets the
    /// cadence of delivery, version re-checks, and cancellation points.
    pub read_chunk_bytes: NonZeroU64,
}

impl Default for DownloadConfig {
    fn default() -> DownloadConfig {
        DownloadConfig {
            priority: DownloadPriority::default(),
            read_chunk_bytes: NonZeroU64::new(DEFAULT_READ_CHUNK_BYTES).unwrap_or(NonZeroU64::MIN),
        }
    }
}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// What kind of request a [`DownloadStep::Submit`] carries — so the driver
/// can arm the network cancel around the download without parsing the
/// payload it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitKind {
    /// Rebind a durable remote locator to this TDLib client's numeric id.
    ResolveRemote,
    /// The ranged `downloadFile`; TDLib is doing network work until it
    /// answers, so an abandoning driver should fire the cancel request.
    Download,
    /// The source-object reference refresh; no download is running.
    Refresh,
}

/// The caller's current obligation, from [`DownloadMachine::next_step`].
/// Idempotent: without an intervening `on_*` call the same obligation is
/// returned again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStep {
    /// Submit this request on the account's client and feed the outcome to
    /// [`DownloadMachine::on_response`].
    Submit {
        /// The serialized-ready request object.
        payload: Value,
        /// What the request is, for cancel arming.
        kind: SubmitKind,
    },
    /// Read exactly `len` bytes at `offset` from TDLib's local file and
    /// feed them to [`DownloadMachine::on_read`] (or the failure to
    /// [`DownloadMachine::on_read_error`]). The file is TDLib's: open it
    /// read-only, never move or delete it (module docs).
    ReadLocal {
        /// TDLib's reported local path.
        path: String,
        /// Absolute offset of the first byte.
        offset: u64,
        /// Bytes to read; never zero.
        len: NonZeroU64,
    },
    /// Every requested byte was read and accepted; the fetch is complete.
    Done,
}

/// Which stage of the fetch the machine is in, as
/// [`DownloadMachine::progress`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    /// Rebinding the durable remote locator to this TDLib client.
    Resolving,
    /// Waiting for the ranged download to complete inside TDLib.
    Downloading,
    /// Refreshing an expired content reference.
    Refreshing,
    /// Reading and delivering local bytes.
    Delivering,
    /// Every byte delivered.
    Complete,
    /// Terminally failed; [`DownloadMachine::next_step`] repeats the error.
    Failed,
}

/// One fetch's observable accounting. Delivery into the sink is the
/// contract's progress signal (SYNC-046); this mirrors it for logs and
/// tests (NFR-033).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Bytes handed to the driver for delivery so far.
    pub delivered: u64,
    /// Total bytes the fetch owes — the range length.
    pub expected: u64,
    /// Where the fetch stands.
    pub phase: DownloadPhase,
}

#[derive(Debug)]
enum Phase {
    /// Resolve the durable remote id into this client's numeric file id.
    ResolveRemote,
    /// Submit the ranged download; awaiting its response.
    Download,
    /// Submit the source-object refresh; awaiting its response.
    Refresh {
        /// Continue the same fetch after an initial remote-id rebind.
        ///
        /// `getRemoteFile` restores the process-local id but not the owner
        /// source TDLib needs to renew a Telegram file reference. Refreshing
        /// the canonical message/story before the first download supplies
        /// that source. A refresh reached after an already-started download
        /// keeps the existing retry classification instead.
        resume_download: bool,
    },
    /// Reading local bytes at `cursor`.
    Read {
        path: String,
    },
    Done,
}

/// The deterministic sans-IO ranged download machine for one fetch. The
/// driver owns the wiring — see [`TdDownloader`] and the module docs.
#[derive(Debug)]
pub struct DownloadMachine {
    /// `None` exactly when the gate failed and `failed` is set.
    target: Option<FileTarget>,
    range: ByteRange,
    pinned: ContentVersion,
    priority: DownloadPriority,
    read_cap: u64,
    phase: Phase,
    /// Next offset to read; delivery is contiguous from `range.start()`.
    cursor: u64,
    /// A `Submit` obligation is unanswered.
    outstanding: bool,
    failed: Option<SourceError>,
    refreshed: Option<RefreshedFileTarget>,
}

impl DownloadMachine {
    /// A machine for one fetch, with the pre-network gates already
    /// evaluated against the resolved catalog `entry` — in the contract's
    /// order: absent item, directory, POL-4 availability, version pin,
    /// extent. A gate failure surfaces from the first
    /// [`next_step`](Self::next_step); nothing reaches the network
    /// (POL-4: a restricted attachment costs zero requests).
    pub fn new(
        request: &FetchRequest,
        entry: Option<CatalogEntry>,
        config: &DownloadConfig,
    ) -> DownloadMachine {
        let mut machine = DownloadMachine {
            target: None,
            range: request.range,
            pinned: request.version.clone(),
            priority: config.priority,
            read_cap: config.read_chunk_bytes.get(),
            phase: Phase::Download,
            cursor: request.range.start(),
            outstanding: false,
            failed: None,
            refreshed: None,
        };
        match gate(&request.range, &request.version, entry) {
            Ok(target) => {
                machine.phase = if target.remote_id.is_some() {
                    Phase::ResolveRemote
                } else {
                    Phase::Download
                };
                machine.target = Some(target);
            }
            Err(error) => machine.failed = Some(error),
        }
        machine
    }

    /// The file this fetch downloads, when the gate passed — what the
    /// driver serializes concurrent fetches on.
    pub fn file_id(&self) -> Option<i32> {
        self.target.as_ref().map(|target| target.file_id)
    }

    /// The `cancelDownloadFile` request that stops this fetch's network
    /// work, for the driver's abandon path. `None` when the gate failed
    /// and no download can have started.
    pub fn cancel_request(&self) -> Option<Value> {
        self.target.as_ref().map(|target| {
            json!({
                "@type": "cancelDownloadFile",
                "file_id": target.file_id,
                "only_if_pending": false,
            })
        })
    }

    /// The fetch's observable accounting.
    pub fn progress(&self) -> DownloadProgress {
        DownloadProgress {
            delivered: self.cursor - self.range.start(),
            expected: self.range.len(),
            phase: if self.failed.is_some() {
                DownloadPhase::Failed
            } else {
                match self.phase {
                    Phase::ResolveRemote => DownloadPhase::Resolving,
                    Phase::Download => DownloadPhase::Downloading,
                    Phase::Refresh { .. } => DownloadPhase::Refreshing,
                    Phase::Read { .. } => DownloadPhase::Delivering,
                    Phase::Done => DownloadPhase::Complete,
                }
            },
        }
    }

    /// Takes locator facts verified during the last refresh response.
    pub fn take_refreshed(&mut self) -> Option<RefreshedFileTarget> {
        self.refreshed.take()
    }

    /// The caller's current obligation. A terminal failure repeats.
    pub fn next_step(&mut self) -> Result<DownloadStep, SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let Some(target) = &self.target else {
            // Unreachable by construction (no failure implies a target);
            // fail closed rather than panic (NFR-030).
            return Err(self.fail(SourceError::Internal {
                detail: "download machine has neither a target nor a failure".to_owned(),
            }));
        };
        match &self.phase {
            Phase::ResolveRemote => {
                self.outstanding = true;
                let Some(remote_file_id) = target.remote_id.as_deref() else {
                    return Err(self.fail(SourceError::Internal {
                        detail: "remote resolution has no durable remote file id".to_owned(),
                    }));
                };
                Ok(DownloadStep::Submit {
                    payload: json!({
                        "@type": "getRemoteFile",
                        "remote_file_id": remote_file_id,
                        "file_type": target.remote_file_type.map(|file_type| {
                            json!({"@type": file_type.as_str()})
                        }),
                    }),
                    kind: SubmitKind::ResolveRemote,
                })
            }
            Phase::Download => {
                self.outstanding = true;
                Ok(DownloadStep::Submit {
                    payload: json!({
                        "@type": "downloadFile",
                        "file_id": target.file_id,
                        "priority": self.priority.get(),
                        "offset": self.range.start(),
                        "limit": self.range.len(),
                        "synchronous": true,
                    }),
                    kind: SubmitKind::Download,
                })
            }
            Phase::Refresh { .. } => {
                self.outstanding = true;
                let payload = match target.refresh {
                    RefreshTarget::Message {
                        chat_id,
                        message_id,
                    } => json!({
                        "@type": "getMessage",
                        "chat_id": chat_id,
                        "message_id": message_id,
                    }),
                    RefreshTarget::Story {
                        poster_chat_id,
                        story_id,
                    } => json!({
                        "@type": "getStory",
                        "story_poster_chat_id": poster_chat_id,
                        "story_id": story_id,
                        "only_local": false,
                    }),
                };
                Ok(DownloadStep::Submit {
                    payload,
                    kind: SubmitKind::Refresh,
                })
            }
            Phase::Read { path } => {
                let remaining = self.range.end() - self.cursor;
                match NonZeroU64::new(remaining.min(self.read_cap)) {
                    Some(len) => Ok(DownloadStep::ReadLocal {
                        path: path.clone(),
                        offset: self.cursor,
                        len,
                    }),
                    None => {
                        self.phase = Phase::Done;
                        Ok(DownloadStep::Done)
                    }
                }
            }
            Phase::Done => Ok(DownloadStep::Done),
        }
    }

    /// Feed the outcome of the request the last [`DownloadStep::Submit`]
    /// named. `Err` is the classified terminal failure of the fetch — the
    /// same error `next_step` will keep returning.
    pub fn on_response(&mut self, outcome: Result<Value, TdError>) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if !self.outstanding {
            return Err(self.fail(SourceError::Internal {
                detail: "a response was fed while no download request was outstanding".to_owned(),
            }));
        }
        self.outstanding = false;
        match &self.phase {
            Phase::ResolveRemote => self.on_remote_outcome(outcome),
            Phase::Download => self.on_download_outcome(outcome),
            Phase::Refresh { resume_download } => {
                self.on_refresh_outcome(outcome, *resume_download)
            }
            Phase::Read { .. } | Phase::Done => Err(self.fail(SourceError::Internal {
                detail: "a response was fed outside a request phase".to_owned(),
            })),
        }
    }

    /// Feed one successful local read of the last
    /// [`DownloadStep::ReadLocal`] — exactly the requested bytes, which the
    /// driver then delivers to the sink.
    pub fn on_read(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let Phase::Read { .. } = &self.phase else {
            return Err(self.fail(SourceError::Internal {
                detail: "a local read was fed outside the read phase".to_owned(),
            }));
        };
        if offset != self.cursor {
            return Err(self.fail(SourceError::Internal {
                detail: format!(
                    "local read at offset {offset} does not match the delivery cursor {}",
                    self.cursor
                ),
            }));
        }
        let expected = (self.range.end() - self.cursor).min(self.read_cap);
        if bytes.len() as u64 != expected {
            // The local file holds less than the download response
            // promised — a cache eviction or an external move. Retryable:
            // the next attempt re-downloads and re-reads fresh state.
            return Err(self.fail(SourceError::Unavailable {
                detail: format!(
                    "TDLib's local file served {} of {expected} bytes at offset {offset}",
                    bytes.len()
                ),
            }));
        }
        self.cursor += expected;
        Ok(())
    }

    /// Feed a failed local read. Always terminal for this attempt; the
    /// classification is retryable, because a fresh attempt re-asks TDLib
    /// for current local state.
    pub fn on_read_error(&mut self, detail: String) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        Err(self.fail(SourceError::Unavailable {
            detail: format!("reading TDLib's local file failed: {detail}"),
        }))
    }

    /// Re-verify the fetch against a fresh catalog resolution — the
    /// mid-fetch half of version verification (SYNC-042). The driver calls
    /// this before every delivered slice; a departed item, flipped
    /// availability, or moved version ends the fetch with the same typed
    /// errors the gate uses.
    pub fn observe_entry(&mut self, entry: Option<&CatalogEntry>) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let error = match entry {
            None => SourceError::NotFound {
                detail: "the item departed mid-fetch".to_owned(),
            },
            Some(CatalogEntry::Directory) => SourceError::InvalidRequest {
                detail: "the item became a directory mid-fetch".to_owned(),
            },
            Some(CatalogEntry::File(target)) => {
                if target.availability != AttachmentAvailability::Fetchable {
                    SourceError::Restricted {
                        detail: "the attachment became restricted mid-fetch (POL-4)".to_owned(),
                    }
                } else if target.version != self.pinned {
                    SourceError::VersionConflict {
                        current: Some(target.version.clone()),
                        detail: format!(
                            "content moved from the pinned {} to {} mid-fetch",
                            self.pinned, target.version
                        ),
                    }
                } else {
                    return Ok(());
                }
            }
        };
        Err(self.fail(error))
    }

    // -- internals ----------------------------------------------------------

    fn fail(&mut self, error: SourceError) -> SourceError {
        self.failed = Some(error.clone());
        error
    }

    fn on_remote_outcome(&mut self, outcome: Result<Value, TdError>) -> Result<(), SourceError> {
        match outcome {
            Ok(file) => match self.verify_remote_file(&file) {
                Ok(refresh) => {
                    if let Some(target) = &mut self.target {
                        target.file_id = refresh.file_id;
                        target.remote_id = refresh.remote_id.clone();
                        target.remote_unique_id = refresh.remote_unique_id.clone();
                        target.size = refresh.size;
                    }
                    // A durable remote id only restores a process-local id.
                    // Re-open the canonical owner so TDLib can associate a
                    // refreshable file source before persisting the rebound
                    // locator or starting the ranged download.
                    self.phase = Phase::Refresh {
                        resume_download: true,
                    };
                    Ok(())
                }
                Err(error) => Err(self.fail(error)),
            },
            Err(error) => Err(self.fail(classify_runtime_error(error, "getRemoteFile"))),
        }
    }

    fn verify_remote_file(&self, file: &Value) -> Result<RefreshedFileTarget, SourceError> {
        let Some(target) = &self.target else {
            return Err(SourceError::Internal {
                detail: "remote-file response without a target".to_owned(),
            });
        };
        if file.get("@type").and_then(Value::as_str) != Some("file") {
            return Err(SourceError::Internal {
                detail: "getRemoteFile answered something other than a file".to_owned(),
            });
        }
        let file_id = file
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| SourceError::Internal {
                detail: "getRemoteFile returned no usable process-local file id".to_owned(),
            })?;
        let remote = file.get("remote").unwrap_or(&Value::Null);
        let remote_id = remote
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if target.remote_id.is_some() && remote_id.is_none() {
            return Err(SourceError::Unavailable {
                detail: "getRemoteFile returned no current durable remote locator".to_owned(),
            });
        }
        let remote_unique_id = remote
            .get("unique_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if let Some(before) = target.remote_unique_id.as_deref()
            && remote_unique_id.as_deref() != Some(before)
        {
            return Err(SourceError::VersionConflict {
                current: Some(self.pinned.clone()),
                detail: "getRemoteFile failed to preserve stable content identity".to_owned(),
            });
        }
        let reported_size = file
            .get("size")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .or_else(|| {
                file.get("expected_size")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0)
            });
        if let (Some(before), Some(after)) = (target.size, reported_size)
            && before != after
        {
            return Err(SourceError::VersionConflict {
                current: Some(self.pinned.clone()),
                detail: format!("getRemoteFile changed the pinned extent from {before} to {after}"),
            });
        }
        Ok(RefreshedFileTarget {
            file_id,
            remote_id,
            remote_unique_id,
            size: reported_size.or(target.size),
            availability: target.availability,
            can_be_saved: true,
        })
    }

    fn on_download_outcome(&mut self, outcome: Result<Value, TdError>) -> Result<(), SourceError> {
        match outcome {
            Ok(file) => match self.validate_download(&file) {
                Ok(path) => {
                    self.phase = Phase::Read { path };
                    Ok(())
                }
                Err(error) => Err(self.fail(error)),
            },
            Err(error) => {
                if is_stale_reference(&error) {
                    // DOM-007: the locator went stale; the refresh
                    // protocol runs before the failure surfaces, so the
                    // caller's retry finds a live reference.
                    self.phase = Phase::Refresh {
                        resume_download: false,
                    };
                    return Ok(());
                }
                Err(self.fail(classify_runtime_error(error, "downloadFile")))
            }
        }
    }

    /// Validate the synchronous `downloadFile` answer: the right file,
    /// an extent the range fits, and local coverage of the whole range.
    fn validate_download(&self, file: &Value) -> Result<String, SourceError> {
        let Some(target) = &self.target else {
            return Err(SourceError::Internal {
                detail: "download response without a target".to_owned(),
            });
        };
        if file.get("@type").and_then(Value::as_str) != Some("file")
            || file.get("id").and_then(Value::as_i64) != Some(i64::from(target.file_id))
        {
            return Err(SourceError::Internal {
                detail: format!(
                    "downloadFile answered something other than file {}",
                    target.file_id
                ),
            });
        }
        // The response is the first authoritative extent for a target the
        // catalog had no size for; TDLib reports 0 while unknown.
        let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
        if size > 0 && self.range.end() > size {
            return Err(SourceError::InvalidRequest {
                detail: format!(
                    "range [{}, {}) runs past the file's {size}-byte extent",
                    self.range.start(),
                    self.range.end()
                ),
            });
        }
        let local = file.get("local").cloned().unwrap_or(Value::Null);
        let completed = local
            .get("is_downloading_completed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let offset = local
            .get("download_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let prefix = local
            .get("downloaded_prefix_size")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let covered = completed
            || (offset <= self.range.start() && offset.saturating_add(prefix) >= self.range.end());
        if !covered {
            // A displaced download position or an interrupted transfer:
            // retryable, and the retry resumes from TDLib's cache.
            return Err(SourceError::Unavailable {
                detail: format!(
                    "synchronous download resolved without covering [{}, {}): \
                     offset {offset}, prefix {prefix}, completed {completed}",
                    self.range.start(),
                    self.range.end()
                ),
            });
        }
        match local.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => Ok(path.to_owned()),
            _ => Err(SourceError::Unavailable {
                detail: "download completed but TDLib reported no local path".to_owned(),
            }),
        }
    }

    /// Fold the source-object refresh answer: verify the refreshed content
    /// still names the pin, then surface
    /// [`SourceError::StaleReference`] — the caller retries against the
    /// now-live reference, and identity never moved (SYNC-045).
    fn on_refresh_outcome(
        &mut self,
        outcome: Result<Value, TdError>,
        resume_download: bool,
    ) -> Result<(), SourceError> {
        let (source_name, refresh_name) = match self.target.as_ref().map(|target| target.refresh) {
            Some(RefreshTarget::Story { .. }) => ("story", "getStory refresh"),
            _ => ("attachment's message", "getMessage refresh"),
        };
        let error = match outcome {
            Ok(message) => match self.verify_refresh(&message) {
                Ok(refresh) => {
                    if let Some(target) = &mut self.target {
                        target.file_id = refresh.file_id;
                        target.remote_id = refresh.remote_id.clone();
                        target.remote_unique_id = refresh.remote_unique_id.clone();
                        target.size = refresh.size;
                    }
                    self.refreshed = Some(refresh);
                    if resume_download {
                        self.phase = Phase::Download;
                        return Ok(());
                    }
                    SourceError::StaleReference {
                        detail: "the content reference expired and was refreshed; retry the fetch"
                            .to_owned(),
                    }
                }
                Err(error) => error,
            },
            Err(error) => {
                if is_source_object_gone(&error) {
                    // TDLib only reports source state here. The provider's
                    // durable item can still be live, so this must not be
                    // promoted to an item-deletion claim.
                    SourceError::Unavailable {
                        detail: format!("the {source_name} is unavailable at the source: {error}"),
                    }
                } else {
                    classify_runtime_error(error, refresh_name)
                }
            }
        };
        Err(self.fail(error))
    }

    fn verify_refresh(&self, value: &Value) -> Result<RefreshedFileTarget, SourceError> {
        let Some(target) = &self.target else {
            return Err(SourceError::Internal {
                detail: "refresh response without a target".to_owned(),
            });
        };
        match target.refresh {
            RefreshTarget::Message {
                chat_id,
                message_id,
            } => self.verify_message_refresh(value, target, chat_id, message_id),
            RefreshTarget::Story {
                poster_chat_id,
                story_id,
            } => self.verify_story_refresh(value, target, poster_chat_id, story_id),
        }
    }

    fn verify_message_refresh(
        &self,
        message: &Value,
        target: &FileTarget,
        chat_id: i64,
        message_id: i64,
    ) -> Result<RefreshedFileTarget, SourceError> {
        let record = match normalize_message(message) {
            Ok(record) => record,
            Err(error) => {
                return Err(SourceError::Internal {
                    detail: format!("the refreshed message did not normalize: {error}"),
                });
            }
        };
        if record.chat_id != chat_id || record.message_id != message_id {
            return Err(SourceError::VersionConflict {
                current: None,
                detail: "getMessage returned a different message identity".to_owned(),
            });
        }
        let Some(descriptor) = record.content.attachment() else {
            return Err(SourceError::VersionConflict {
                current: None,
                detail: "the refreshed message no longer carries the attachment".to_owned(),
            });
        };
        if descriptor.availability != AttachmentAvailability::Fetchable {
            return Err(SourceError::Restricted {
                detail: "the attachment is restricted as of the refresh (POL-4)".to_owned(),
            });
        }
        if !record.can_be_saved {
            return Err(SourceError::Restricted {
                detail: "the refreshed message can no longer be saved (POL-4)".to_owned(),
            });
        }
        let file_id = descriptor.file_id.ok_or_else(|| SourceError::Unavailable {
            detail: "the refreshed message has no process-local TDLib file id".to_owned(),
        })?;
        if let Some(before) = target.remote_unique_id.as_deref() {
            match descriptor.remote_unique_id.as_deref() {
                Some(after) if before == after => {}
                Some(after) => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: format!(
                            "the refreshed message carries different content \
                             (remote unique id {before} became {after})"
                        ),
                    });
                }
                None => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: "the refreshed message lost the pinned stable content identity"
                            .to_owned(),
                    });
                }
            }
        }
        if let Some(before) = target.size {
            match descriptor.size {
                Some(after) if before == after => {}
                Some(after) => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: format!(
                            "the refreshed message changed the pinned extent from {before} to {after}"
                        ),
                    });
                }
                None => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: "the refreshed message lost the pinned content extent".to_owned(),
                    });
                }
            }
        }
        Ok(RefreshedFileTarget {
            file_id,
            remote_id: descriptor.remote_id.clone(),
            remote_unique_id: descriptor.remote_unique_id.clone(),
            size: descriptor.size,
            availability: descriptor.availability,
            can_be_saved: record.can_be_saved,
        })
    }

    fn verify_story_refresh(
        &self,
        value: &Value,
        target: &FileTarget,
        poster_chat_id: i64,
        story_id: i64,
    ) -> Result<RefreshedFileTarget, SourceError> {
        let story = normalize_story(value).map_err(|_| SourceError::Internal {
            detail: "the refreshed story did not normalize".to_owned(),
        })?;
        if story.poster_chat_id != poster_chat_id || story.story_id != story_id {
            return Err(SourceError::VersionConflict {
                current: None,
                detail: "getStory returned a different canonical story identity".to_owned(),
            });
        }
        if !story.can_be_forwarded {
            return Err(SourceError::Restricted {
                detail: "the refreshed story can no longer be saved (POL-4)".to_owned(),
            });
        }
        let locator = story
            .locators
            .iter()
            .find(|locator| locator.is_primary)
            .ok_or_else(|| SourceError::VersionConflict {
                current: None,
                detail: "the refreshed story no longer carries supported primary content"
                    .to_owned(),
            })?;
        let file_id = locator
            .local_file_id
            .ok_or_else(|| SourceError::Unavailable {
                detail: "the refreshed story has no process-local TDLib file id".to_owned(),
            })?;
        if let Some(before) = target.remote_unique_id.as_deref() {
            match locator.remote_unique_id.as_deref() {
                Some(after) if before == after => {}
                Some(after) => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: format!(
                            "the refreshed story carries different content \
                             (remote unique id {before} became {after})"
                        ),
                    });
                }
                None => {
                    return Err(SourceError::VersionConflict {
                        current: None,
                        detail: "the refreshed story lost the pinned stable content identity"
                            .to_owned(),
                    });
                }
            }
        }
        if let Some(before) = target.size {
            let after = locator.size.or(locator.expected_size);
            if after != Some(before) {
                return Err(SourceError::VersionConflict {
                    current: None,
                    detail: format!(
                        "the refreshed story changed or lost the pinned extent {before}"
                    ),
                });
            }
        }
        Ok(RefreshedFileTarget {
            file_id,
            remote_id: locator.remote_file_id.clone(),
            remote_unique_id: locator.remote_unique_id.clone(),
            size: locator.size.or(locator.expected_size),
            availability: AttachmentAvailability::Fetchable,
            can_be_saved: true,
        })
    }
}

/// The pre-network gates, in the contract's order (mirroring the
/// deterministic fake, the contract's executable specification).
fn gate(
    range: &ByteRange,
    pinned: &ContentVersion,
    entry: Option<CatalogEntry>,
) -> Result<FileTarget, SourceError> {
    let target = match entry {
        None => {
            return Err(SourceError::NotFound {
                detail: "no item with this identity".to_owned(),
            });
        }
        Some(CatalogEntry::Directory) => {
            return Err(SourceError::InvalidRequest {
                detail: "cannot fetch content of a directory".to_owned(),
            });
        }
        Some(CatalogEntry::File(target)) => target,
    };
    if target.availability != AttachmentAvailability::Fetchable {
        if target.availability == AttachmentAvailability::Unavailable {
            return Err(SourceError::Unavailable {
                detail: "the attachment has no usable Telegram file locator".to_owned(),
            });
        }
        return Err(SourceError::Restricted {
            detail: match target.availability {
                AttachmentAvailability::Restricted => {
                    "the attachment is save-restricted (POL-4); its bytes are never fetched"
                }
                AttachmentAvailability::ViewOnce => {
                    "the attachment is view-once (POL-4); its bytes are never persisted"
                }
                AttachmentAvailability::Unavailable => "unreachable",
                AttachmentAvailability::Fetchable => "unreachable",
            }
            .to_owned(),
        });
    }
    if target.version != *pinned {
        return Err(SourceError::VersionConflict {
            current: Some(target.version.clone()),
            detail: format!(
                "the fetch is pinned to {pinned} but the source serves {}",
                target.version
            ),
        });
    }
    if let Some(size) = target.size
        && range.end() > size
    {
        return Err(SourceError::InvalidRequest {
            detail: format!(
                "range [{}, {}) runs past the file's {size}-byte extent",
                range.start(),
                range.end()
            ),
        });
    }
    Ok(target)
}

/// Whether a TDLib rejection is Telegram's stale-file-reference class
/// (`FILE_REFERENCE_EXPIRED` and friends).
pub(crate) fn is_stale_reference(error: &TdError) -> bool {
    matches!(error, TdError::Td { message, .. } if message.contains("FILE_REFERENCE"))
}

/// Whether a source-object refresh says the message or story is unavailable.
/// This is source evidence only: deletion remains a durable-state decision.
fn is_source_object_gone(error: &TdError) -> bool {
    matches!(
        error,
        TdError::Td { code, message }
            if *code == 404 || message.contains("MESSAGE_ID_INVALID") || message == "Not Found"
    )
}

/// Normalize a tdjson failure into the provider-neutral taxonomy (DEC-003:
/// no TDLib error type crosses the source boundary). Flood waits carry
/// their stated delay intact — the one number the engine can act on
/// (SYNC-044).
pub(crate) fn classify_runtime_error(error: TdError, operation: &str) -> SourceError {
    if let Some(retry_after_secs) = retryable_after(&error) {
        return match retry_after_secs {
            Some(secs) => SourceError::RateLimited {
                retry_after: Some(Duration::from_secs(secs)),
                detail: format!("{operation}: {error}"),
            },
            None => SourceError::Unavailable {
                detail: format!("{operation}: {error}"),
            },
        };
    }
    match error {
        TdError::Td { code: 401, message } => SourceError::AuthRequired {
            detail: format!("{operation}: TDLib error 401: {message}"),
        },
        error @ TdError::Td { .. } => SourceError::Internal {
            detail: format!("{operation}: unclassified TDLib rejection: {error}"),
        },
        TdError::ClientClosed | TdError::Shutdown => SourceError::Unavailable {
            detail: format!("{operation}: the tdjson runtime is gone: {error}"),
        },
        error @ (TdError::InvalidRequest { .. } | TdError::Protocol { .. }) => {
            SourceError::Internal {
                detail: format!("{operation}: {error}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-file serialization
// ---------------------------------------------------------------------------

/// One download conversation per file (module docs): fetches serialize per
/// `file_id`, waiters wake on release and re-race deterministically under
/// a single-threaded executor. Shared with the thumbnail adapter
/// ([`crate::thumbnail`]), which serializes its whole-file preview downloads
/// on the same per-`file_id` discipline.
#[derive(Debug, Default)]
pub(crate) struct FileLocks {
    table: Arc<LockTable>,
}

#[derive(Debug, Default)]
struct LockTable {
    slots: Mutex<HashMap<i32, LockSlot>>,
}

#[derive(Debug, Default)]
struct LockSlot {
    held: bool,
    waiters: Vec<Waker>,
}

impl LockTable {
    // Poison recovery as in the runtime: the map is valid at every unlock
    // point, and a panic elsewhere must not wedge unrelated fetches.
    fn lock_slots(&self) -> MutexGuard<'_, HashMap<i32, LockSlot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl FileLocks {
    pub(crate) fn acquire(&self, file_id: i32) -> LockFuture {
        LockFuture {
            table: Arc::clone(&self.table),
            file_id,
        }
    }
}

pub(crate) struct LockFuture {
    table: Arc<LockTable>,
    file_id: i32,
}

impl Future for LockFuture {
    type Output = LockGuard;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<LockGuard> {
        let mut slots = self.table.lock_slots();
        let slot = slots.entry(self.file_id).or_default();
        if slot.held {
            if !slot.waiters.iter().any(|w| w.will_wake(context.waker())) {
                slot.waiters.push(context.waker().clone());
            }
            Poll::Pending
        } else {
            slot.held = true;
            drop(slots);
            Poll::Ready(LockGuard {
                table: Arc::clone(&self.table),
                file_id: self.file_id,
            })
        }
    }
}

pub(crate) struct LockGuard {
    table: Arc<LockTable>,
    file_id: i32,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let waiters = {
            let mut slots = self.table.lock_slots();
            match slots.get_mut(&self.file_id) {
                Some(slot) => {
                    slot.held = false;
                    let waiters = std::mem::take(&mut slot.waiters);
                    if waiters.is_empty() {
                        slots.remove(&self.file_id);
                    }
                    waiters
                }
                None => Vec::new(),
            }
        };
        // Wake outside the lock; woken futures re-poll and re-race.
        for waker in waiters {
            waker.wake();
        }
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Fires `cancelDownloadFile` if the fetch is abandoned while TDLib is
/// downloading — the SYNC-043 "cease network work" half of a dropped
/// future. Disarmed the moment the download resolves; the response of the
/// fired cancel is deliberately discarded (the abandoning caller is gone).
/// Shared with the thumbnail adapter ([`crate::thumbnail`]), whose preview
/// download abandons the same way.
pub(crate) struct CancelGuard {
    client: TdClient,
    request: Option<Value>,
}

impl CancelGuard {
    pub(crate) fn disarmed(client: TdClient) -> CancelGuard {
        CancelGuard {
            client,
            request: None,
        }
    }

    pub(crate) fn arm(&mut self, request: Option<Value>) {
        self.request = request;
    }

    pub(crate) fn disarm(&mut self) {
        self.request = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            // Submission is synchronous; dropping the handle discards the
            // answer, which is exactly right for an abandoned fetch.
            drop(self.client.request(request));
        }
    }
}

/// The ranged download driver: `DriveSource::fetch`'s implementation for
/// the tdjson source, shaped for the full adapter to delegate to (module
/// docs).
pub struct TdDownloader {
    client: TdClient,
    catalog: Arc<dyn FetchCatalog>,
    config: DownloadConfig,
    locks: FileLocks,
}

impl std::fmt::Debug for TdDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdDownloader")
            .field("client", &self.client)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TdDownloader {
    /// A downloader submitting on `client` and resolving items through
    /// `catalog`.
    pub fn new(
        client: TdClient,
        catalog: Arc<dyn FetchCatalog>,
        config: DownloadConfig,
    ) -> TdDownloader {
        TdDownloader {
            client,
            catalog,
            config,
            locks: FileLocks::default(),
        }
    }

    /// Delivers exactly `request.range` of `request.item` into `sink`,
    /// pinned to `request.version` — `DriveSource::fetch`'s contract
    /// (`gramdrive_source::fetch`), implemented over TDLib downloads.
    pub fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> Pin<Box<dyn Future<Output = Result<(), SourceError>> + Send + 'a>> {
        Box::pin(async move { self.fetch_inner(request, sink).await })
    }

    async fn fetch_inner(
        &self,
        request: FetchRequest,
        sink: &mut dyn ContentSink,
    ) -> Result<(), SourceError> {
        let entry = self.catalog.resolve(&request.item);
        let mut machine = DownloadMachine::new(&request, entry, &self.config);
        // A durable remote id is resolved before serialization because the
        // catalog's numeric id may belong to an earlier TDLib client.
        let mut serialized: Option<LockGuard> = None;
        let mut cancel = CancelGuard::disarmed(self.client.clone());
        loop {
            match machine.next_step()? {
                DownloadStep::Submit { payload, kind } => {
                    if kind == SubmitKind::Download {
                        if serialized.is_none() {
                            let file_id =
                                machine.file_id().ok_or_else(|| SourceError::Internal {
                                    detail: "download has no process-local file id".to_owned(),
                                })?;
                            serialized = Some(self.locks.acquire(file_id).await);
                        }
                        cancel.arm(machine.cancel_request());
                    }
                    let outcome = match self.client.request(payload) {
                        Ok(pending) => pending.await,
                        Err(error) => Err(error),
                    };
                    cancel.disarm();
                    let response = machine.on_response(outcome);
                    if matches!(kind, SubmitKind::ResolveRemote | SubmitKind::Refresh)
                        && let Some(refresh) = machine.take_refreshed()
                    {
                        self.catalog.persist_refresh(&request.item, &refresh)?;
                    }
                    response?;
                }
                DownloadStep::ReadLocal { path, offset, len } => {
                    // The mid-fetch half of version verification: the pin
                    // is re-checked before every delivered slice
                    // (SYNC-042).
                    machine.observe_entry(self.catalog.resolve(&request.item).as_ref())?;
                    match read_exact_at(&path, offset, len.get()) {
                        Ok(bytes) => {
                            machine.on_read(offset, &bytes)?;
                            let chunk = ContentChunk::new(offset, &bytes).map_err(|invalid| {
                                SourceError::Internal {
                                    detail: format!("delivery chunk failed to form: {invalid}"),
                                }
                            })?;
                            if sink.accept(chunk) == SinkControl::Stop {
                                return Err(SourceError::Cancelled {
                                    detail: "the sink stopped delivery (SYNC-043)".to_owned(),
                                });
                            }
                            // A cancellation point between slices: an
                            // abandoning caller drops the future here
                            // (SYNC-005), and no slice blocks the executor
                            // for longer than one read.
                            yield_once().await;
                        }
                        Err(error) => {
                            machine.on_read_error(format!("{path} at offset {offset}: {error}"))?;
                        }
                    }
                }
                DownloadStep::Done => return Ok(()),
            }
        }
    }
}

impl ContentSource for TdDownloader {
    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        TdDownloader::fetch(self, request, sink)
    }
}

/// Read exactly `len` bytes at `offset` from TDLib's file, read-only. A
/// short file is an error — the download response promised coverage. Shared
/// with the thumbnail adapter ([`crate::thumbnail`]), which reads a whole
/// preview file as one `offset == 0` slice.
pub(crate) fn read_exact_at(path: &str, offset: u64, len: u64) -> Result<Vec<u8>, std::io::Error> {
    let len = usize::try_from(len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "read slice exceeds the address space",
        )
    })?;
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Yields to the executor exactly once, waking itself first — drivable by
/// a noop-waker poll loop and by a real runtime alike (the `yield_now`
/// pattern; see `gramdrive-testkit`'s executor notes).
fn yield_once() -> impl Future<Output = ()> {
    let mut yielded = false;
    std::future::poll_fn(move |context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    const FILE_ID: i32 = 700;
    const CHAT_ID: i64 = -10_042;
    const MESSAGE_ID: i64 = 9001;

    fn version(token: &str) -> ContentVersion {
        ContentVersion::new(token).expect("valid token")
    }

    fn target() -> FileTarget {
        FileTarget {
            file_id: FILE_ID,
            remote_id: None,
            remote_file_type: None,
            refresh: RefreshTarget::Message {
                chat_id: CHAT_ID,
                message_id: MESSAGE_ID,
            },
            availability: AttachmentAvailability::Fetchable,
            remote_unique_id: Some("unique-1".to_owned()),
            size: Some(100),
            version: version("c1"),
        }
    }

    fn item() -> ItemId {
        use gramdrive_model::identity::{
            AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey,
            ChatId, ChatKey, ItemKey, MessageId, MessageKey, NamespaceVersion,
        };
        ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: AccountScope {
                        account: AccountKey {
                            account_id: AccountId(7),
                        },
                        namespace_version: NamespaceVersion(1),
                    },
                    chat_id: ChatId(CHAT_ID),
                },
                message_id: MessageId(MESSAGE_ID),
            },
            index: AttachmentIndex(0),
        }))
        .id()
    }

    fn request(start: u64, end: u64) -> FetchRequest {
        FetchRequest {
            item: item(),
            version: version("c1"),
            range: ByteRange::new(start, end).expect("valid range"),
        }
    }

    fn config() -> DownloadConfig {
        DownloadConfig {
            priority: DownloadPriority::new(7).expect("7 is in range"),
            read_chunk_bytes: NonZeroU64::new(16).expect("non-zero"),
        }
    }

    fn machine(entry: Option<CatalogEntry>) -> DownloadMachine {
        DownloadMachine::new(&request(0, 64), entry, &config())
    }

    fn file_response(size: u64, offset: u64, prefix: u64, completed: bool, path: &str) -> Value {
        json!({
            "@type": "file",
            "id": FILE_ID,
            "size": size,
            "local": {
                "@type": "localFile",
                "path": path,
                "download_offset": offset,
                "downloaded_prefix_size": prefix,
                "is_downloading_active": false,
                "is_downloading_completed": completed,
            },
        })
    }

    fn refreshed_message(unique_id: &str, can_be_saved: bool) -> Value {
        refreshed_message_facts(Some(unique_id), Some(100), can_be_saved)
    }

    fn refreshed_message_facts(
        unique_id: Option<&str>,
        size: Option<u64>,
        can_be_saved: bool,
    ) -> Value {
        json!({
            "@type": "message",
            "id": MESSAGE_ID,
            "chat_id": CHAT_ID,
            "date": 1_752_800_000,
            "sender_id": {"@type": "messageSenderUser", "user_id": 42},
            "can_be_saved": can_be_saved,
            "content": {
                "@type": "messageDocument",
                "caption": {"text": ""},
                "document": {
                    "file_name": "payload.bin",
                    "mime_type": "application/octet-stream",
                    "document": {
                        "id": FILE_ID,
                        "size": size,
                        "remote": {"id": "r-1", "unique_id": unique_id},
                    },
                },
            },
        })
    }

    // -- priority -----------------------------------------------------------

    #[test]
    fn priority_accepts_exactly_tdlibs_range() {
        assert_eq!(DownloadPriority::new(0), Err(InvalidPriority { value: 0 }));
        assert_eq!(DownloadPriority::new(1), Ok(DownloadPriority::MIN));
        assert_eq!(DownloadPriority::new(32), Ok(DownloadPriority::MAX));
        assert_eq!(
            DownloadPriority::new(33),
            Err(InvalidPriority { value: 33 })
        );
        assert_eq!(DownloadPriority::default().get(), 16);
        assert_eq!(
            InvalidPriority { value: 40 }.to_string(),
            "download priority 40 is outside TDLib's 1..=32"
        );
    }

    // -- the pre-network gates ----------------------------------------------

    #[test]
    fn gate_rejects_an_absent_item_as_not_found() {
        let mut machine = machine(None);
        assert!(matches!(
            machine.next_step(),
            Err(SourceError::NotFound { .. })
        ));
        assert_eq!(machine.file_id(), None, "no target, so nothing to lock");
        assert_eq!(machine.progress().phase, DownloadPhase::Failed);
    }

    #[test]
    fn gate_rejects_a_directory_as_invalid_request() {
        let mut machine = machine(Some(CatalogEntry::Directory));
        assert!(matches!(
            machine.next_step(),
            Err(SourceError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn gate_rejects_restricted_and_view_once_before_any_request() {
        for availability in [
            AttachmentAvailability::Restricted,
            AttachmentAvailability::ViewOnce,
        ] {
            let mut machine = machine(Some(CatalogEntry::File(FileTarget {
                availability,
                ..target()
            })));
            // POL-4: the typed rejection is the first and only step; no
            // Submit obligation ever exists for this fetch.
            assert!(
                matches!(machine.next_step(), Err(SourceError::Restricted { .. })),
                "{availability:?} must be refused"
            );
        }
    }

    #[test]
    fn gate_rejects_a_stale_pin_with_the_current_version() {
        let mut machine = machine(Some(CatalogEntry::File(FileTarget {
            version: version("c2"),
            ..target()
        })));
        match machine.next_step() {
            Err(SourceError::VersionConflict { current, .. }) => {
                assert_eq!(current, Some(version("c2")));
            }
            other => panic!("expected a version conflict, got {other:?}"),
        }
    }

    #[test]
    fn gate_rejects_a_range_past_a_known_extent() {
        let machine = DownloadMachine::new(
            &request(90, 128),
            Some(CatalogEntry::File(target())), // size 100
            &config(),
        );
        let mut machine = machine;
        assert!(matches!(
            machine.next_step(),
            Err(SourceError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn gate_defers_the_extent_check_when_size_is_unknown() {
        let mut machine = DownloadMachine::new(
            &request(90, 128),
            Some(CatalogEntry::File(FileTarget {
                size: None,
                ..target()
            })),
            &config(),
        );
        assert!(
            matches!(machine.next_step(), Ok(DownloadStep::Submit { .. })),
            "an unknown extent downloads; the response settles it"
        );
    }

    #[test]
    fn a_gate_failure_repeats_and_never_recovers() {
        let mut machine = machine(None);
        let first = machine.next_step().expect_err("gate failed");
        let second = machine.next_step().expect_err("still failed");
        assert_eq!(first, second);
    }

    // -- the download request -----------------------------------------------

    #[test]
    fn the_download_request_carries_range_priority_and_synchronous() {
        let mut machine = DownloadMachine::new(
            &request(512, 1024),
            Some(CatalogEntry::File(FileTarget {
                size: Some(4096),
                ..target()
            })),
            &config(),
        );
        let Ok(DownloadStep::Submit { payload, kind }) = machine.next_step() else {
            panic!("the first obligation is the download");
        };
        assert_eq!(kind, SubmitKind::Download);
        assert_eq!(
            payload,
            json!({
                "@type": "downloadFile",
                "file_id": FILE_ID,
                "priority": 7,
                "offset": 512,
                "limit": 512,
                "synchronous": true,
            })
        );
        assert_eq!(machine.progress().phase, DownloadPhase::Downloading);
        // The obligation repeats until the response is fed.
        assert!(matches!(
            machine.next_step(),
            Ok(DownloadStep::Submit {
                kind: SubmitKind::Download,
                ..
            })
        ));
    }

    #[test]
    fn cancel_request_names_the_file() {
        let machine = machine(Some(CatalogEntry::File(target())));
        assert_eq!(
            machine.cancel_request(),
            Some(json!({
                "@type": "cancelDownloadFile",
                "file_id": FILE_ID,
                "only_if_pending": false,
            }))
        );
    }

    // -- download response validation ----------------------------------------

    fn into_read_phase(machine: &mut DownloadMachine, response: Value) {
        let _ = machine.next_step().expect("submit");
        machine
            .on_response(Ok(response))
            .expect("the response validates");
    }

    #[test]
    fn a_covering_response_moves_to_reads_in_capped_slices() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_read_phase(
            &mut machine,
            file_response(100, 0, 64, false, "/td/file.bin"),
        );

        // 64 bytes at a 16-byte cap: four slices, contiguous.
        for slice in 0u64..4 {
            let step = machine.next_step().expect("a read obligation");
            assert_eq!(
                step,
                DownloadStep::ReadLocal {
                    path: "/td/file.bin".to_owned(),
                    offset: slice * 16,
                    len: NonZeroU64::new(16).expect("non-zero"),
                }
            );
            machine
                .on_read(slice * 16, &[0u8; 16])
                .expect("the slice accounts");
        }
        assert_eq!(machine.next_step(), Ok(DownloadStep::Done));
        let progress = machine.progress();
        assert_eq!(progress.delivered, 64);
        assert_eq!(progress.expected, 64);
        assert_eq!(progress.phase, DownloadPhase::Complete);
    }

    #[test]
    fn a_completed_download_covers_any_range() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_read_phase(&mut machine, file_response(100, 0, 0, true, "/td/file.bin"));
        assert!(matches!(
            machine.next_step(),
            Ok(DownloadStep::ReadLocal { .. })
        ));
    }

    #[test]
    fn a_non_covering_response_is_unavailable() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Ok(file_response(100, 0, 32, false, "/td/file.bin")))
            .expect_err("32 of 64 bytes is not coverage");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_response_for_the_wrong_file_is_internal() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let _ = machine.next_step().expect("submit");
        let mut response = file_response(100, 0, 64, false, "/td/file.bin");
        response["id"] = json!(FILE_ID + 1);
        let error = machine
            .on_response(Ok(response))
            .expect_err("the wrong file cannot serve this fetch");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    #[test]
    fn a_response_without_a_path_is_unavailable() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Ok(file_response(100, 0, 64, false, "")))
            .expect_err("no path, no bytes");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn the_response_extent_rejects_a_range_the_catalog_could_not() {
        // The catalog knew no size; the download response knows 40 bytes,
        // and the 64-byte request runs past it.
        let mut machine = DownloadMachine::new(
            &request(0, 64),
            Some(CatalogEntry::File(FileTarget {
                size: None,
                ..target()
            })),
            &config(),
        );
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Ok(file_response(40, 0, 40, true, "/td/file.bin")))
            .expect_err("the range runs past the discovered extent");
        assert!(matches!(error, SourceError::InvalidRequest { .. }));
    }

    // -- failure classification ----------------------------------------------

    fn download_error(code: i64, message: &str) -> Result<(), SourceError> {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let _ = machine.next_step().expect("submit");
        machine.on_response(Err(TdError::Td {
            code,
            message: message.to_owned(),
        }))
    }

    #[test]
    fn a_flood_wait_carries_its_stated_delay() {
        let error = download_error(429, "Too Many Requests: retry after 17").expect_err("flood");
        assert_eq!(
            error,
            SourceError::RateLimited {
                retry_after: Some(Duration::from_secs(17)),
                detail: "downloadFile: TDLib error 429: Too Many Requests: retry after 17"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_transport_failure_is_unavailable() {
        let error = download_error(500, "Failed to connect").expect_err("transport");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn lost_authorization_is_auth_required() {
        let error = download_error(401, "Unauthorized").expect_err("auth");
        assert!(matches!(error, SourceError::AuthRequired { .. }));
    }

    #[test]
    fn an_unclassified_rejection_is_internal() {
        let error = download_error(400, "FILE_ID_INVALID").expect_err("bad locator");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    #[test]
    fn a_gone_runtime_is_unavailable() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Err(TdError::Shutdown))
            .expect_err("shutdown");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    // -- the reference refresh ----------------------------------------------

    fn into_refresh(machine: &mut DownloadMachine) {
        let _ = machine.next_step().expect("submit download");
        machine
            .on_response(Err(TdError::Td {
                code: 400,
                message: "FILE_REFERENCE_EXPIRED".to_owned(),
            }))
            .expect("a stale reference turns into the refresh, not a failure");
    }

    #[test]
    fn a_stale_reference_refreshes_then_surfaces_stale_reference() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        assert_eq!(machine.progress().phase, DownloadPhase::Refreshing);

        let Ok(DownloadStep::Submit { payload, kind }) = machine.next_step() else {
            panic!("the refresh is a request obligation");
        };
        assert_eq!(kind, SubmitKind::Refresh);
        assert_eq!(
            payload,
            json!({
                "@type": "getMessage",
                "chat_id": CHAT_ID,
                "message_id": MESSAGE_ID,
            })
        );

        // The refreshed message still names the pinned content: the call
        // resolves StaleReference, and the caller's retry finds a live
        // reference (SYNC-045 — nothing about identity moved).
        let error = machine
            .on_response(Ok(refreshed_message("unique-1", true)))
            .expect_err("the refresh surfaces the stale class");
        assert!(matches!(error, SourceError::StaleReference { .. }));
    }

    #[test]
    fn a_stale_story_reference_refreshes_without_opening_or_viewing_the_story() {
        let mut story_target = target();
        story_target.refresh = RefreshTarget::Story {
            poster_chat_id: CHAT_ID,
            story_id: 91,
        };
        let mut machine = machine(Some(CatalogEntry::File(story_target)));
        into_refresh(&mut machine);

        let Ok(DownloadStep::Submit { payload, kind }) = machine.next_step() else {
            panic!("the story refresh is a request obligation");
        };
        assert_eq!(kind, SubmitKind::Refresh);
        assert_eq!(
            payload,
            json!({
                "@type": "getStory",
                "story_poster_chat_id": CHAT_ID,
                "story_id": 91,
                "only_local": false,
            })
        );
        assert_ne!(payload["@type"], "openStory");

        let error = machine
            .on_response(Ok(json!({
                "@type": "story",
                "id": 91,
                "poster_chat_id": CHAT_ID,
                "date": 1_784_692_800,
                "is_posted_to_chat_page": true,
                "can_be_forwarded": true,
                "content": {
                    "@type": "storyContentVideo",
                    "video": {
                        "@type": "storyVideo",
                        "video": {
                            "@type": "file",
                            "id": FILE_ID,
                            "size": 100,
                            "remote": {
                                "@type": "remoteFile",
                                "id": "remote-refreshed",
                                "unique_id": "unique-1"
                            }
                        }
                    }
                }
            })))
            .expect_err("a verified story refresh asks the caller to retry");
        assert!(matches!(error, SourceError::StaleReference { .. }));
        assert_eq!(
            machine.take_refreshed(),
            Some(RefreshedFileTarget {
                file_id: FILE_ID,
                remote_id: Some("remote-refreshed".to_owned()),
                remote_unique_id: Some("unique-1".to_owned()),
                size: Some(100),
                availability: AttachmentAvailability::Fetchable,
                can_be_saved: true,
            })
        );
    }

    #[test]
    fn a_refresh_that_finds_different_content_is_a_version_conflict() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let error = machine
            .on_response(Ok(refreshed_message("unique-2", true)))
            .expect_err("different content is not a refresh");
        assert!(matches!(
            error,
            SourceError::VersionConflict { current: None, .. }
        ));
    }

    #[test]
    fn a_refresh_that_loses_known_stable_identity_is_a_version_conflict() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let error = machine
            .on_response(Ok(refreshed_message_facts(None, Some(100), true)))
            .expect_err("missing stable identity cannot prove the pin");
        assert!(matches!(error, SourceError::VersionConflict { .. }));
    }

    #[test]
    fn a_refresh_that_loses_known_extent_is_a_version_conflict() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let error = machine
            .on_response(Ok(refreshed_message_facts(Some("unique-1"), None, true)))
            .expect_err("missing extent cannot prove the pin");
        assert!(matches!(error, SourceError::VersionConflict { .. }));
    }

    #[test]
    fn a_refresh_that_finds_restricted_content_fails_closed() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let error = machine
            .on_response(Ok(refreshed_message("unique-1", false)))
            .expect_err("restricted as of the refresh");
        assert!(matches!(error, SourceError::Restricted { .. }));
    }

    #[test]
    fn a_refresh_whose_message_is_gone_is_retryable_source_unavailable() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let error = machine
            .on_response(Err(TdError::Td {
                code: 400,
                message: "MESSAGE_ID_INVALID".to_owned(),
            }))
            .expect_err("the message is gone");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_refresh_without_an_attachment_is_a_version_conflict() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_refresh(&mut machine);
        let _ = machine.next_step().expect("submit refresh");
        let text = json!({
            "@type": "message",
            "id": MESSAGE_ID,
            "chat_id": CHAT_ID,
            "date": 1_752_800_000,
            "sender_id": {"@type": "messageSenderUser", "user_id": 42},
            "can_be_saved": true,
            "content": {"@type": "messageText", "text": {"text": "edited away"}},
        });
        let error = machine
            .on_response(Ok(text))
            .expect_err("the attachment is gone from the message");
        assert!(matches!(
            error,
            SourceError::VersionConflict { current: None, .. }
        ));
    }

    // -- mid-fetch verification ----------------------------------------------

    #[test]
    fn observe_entry_passes_while_the_world_holds_still() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        machine
            .observe_entry(Some(&CatalogEntry::File(target())))
            .expect("nothing moved");
    }

    #[test]
    fn observe_entry_fails_the_fetch_when_the_version_moves() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let moved = CatalogEntry::File(FileTarget {
            version: version("c2"),
            ..target()
        });
        match machine.observe_entry(Some(&moved)) {
            Err(SourceError::VersionConflict { current, .. }) => {
                assert_eq!(current, Some(version("c2")));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Terminal: the machine repeats the conflict.
        assert!(matches!(
            machine.next_step(),
            Err(SourceError::VersionConflict { .. })
        ));
    }

    #[test]
    fn observe_entry_fails_a_departed_or_restricted_item() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        assert!(matches!(
            machine.observe_entry(None),
            Err(SourceError::NotFound { .. })
        ));

        let mut machine = DownloadMachine::new(
            &request(0, 64),
            Some(CatalogEntry::File(target())),
            &config(),
        );
        let restricted = CatalogEntry::File(FileTarget {
            availability: AttachmentAvailability::Restricted,
            ..target()
        });
        assert!(matches!(
            machine.observe_entry(Some(&restricted)),
            Err(SourceError::Restricted { .. })
        ));
    }

    // -- read accounting -----------------------------------------------------

    #[test]
    fn a_short_read_is_unavailable_not_silent() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_read_phase(
            &mut machine,
            file_response(100, 0, 64, false, "/td/file.bin"),
        );
        let _ = machine.next_step().expect("a read obligation");
        let error = machine
            .on_read(0, &[0u8; 5])
            .expect_err("5 of 16 bytes is a truncated file");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_read_at_the_wrong_offset_is_internal() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_read_phase(
            &mut machine,
            file_response(100, 0, 64, false, "/td/file.bin"),
        );
        let error = machine
            .on_read(32, &[0u8; 16])
            .expect_err("the cursor is at 0");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    #[test]
    fn a_read_error_is_unavailable() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        into_read_phase(
            &mut machine,
            file_response(100, 0, 64, false, "/td/file.bin"),
        );
        let error = machine
            .on_read_error("permission denied".to_owned())
            .expect_err("a failed read fails the attempt");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_response_with_nothing_outstanding_is_internal() {
        let mut machine = machine(Some(CatalogEntry::File(target())));
        let error = machine
            .on_response(Ok(json!({"@type": "ok"})))
            .expect_err("nothing was submitted");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    // -- the per-file lock ---------------------------------------------------

    #[test]
    fn the_file_lock_serializes_and_wakes_waiters() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;

        struct CountingWaker(AtomicUsize);
        impl Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let locks = FileLocks::default();
        let counting = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counting));
        let mut context = Context::from_waker(&waker);

        let mut first = Box::pin(locks.acquire(FILE_ID));
        let Poll::Ready(guard) = first.as_mut().poll(&mut context) else {
            panic!("an uncontended lock acquires immediately");
        };

        let mut second = Box::pin(locks.acquire(FILE_ID));
        assert!(
            second.as_mut().poll(&mut context).is_pending(),
            "the file is held; the second fetch waits"
        );
        let mut other = Box::pin(locks.acquire(FILE_ID + 1));
        assert!(
            other.as_mut().poll(&mut context).is_ready(),
            "a different file does not serialize"
        );

        assert_eq!(counting.0.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(
            counting.0.load(Ordering::SeqCst),
            1,
            "releasing wakes the waiter"
        );
        assert!(
            second.as_mut().poll(&mut context).is_ready(),
            "the woken waiter acquires"
        );
    }

    #[test]
    fn the_lock_table_forgets_idle_files() {
        let locks = FileLocks::default();
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(locks.acquire(FILE_ID));
        let Poll::Ready(guard) = future.as_mut().poll(&mut context) else {
            panic!("uncontended");
        };
        drop(guard);
        assert!(
            locks.table.lock_slots().is_empty(),
            "a released, waiterless file leaves no entry behind"
        );
    }
}
