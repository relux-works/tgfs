//! The TDLib thumbnail adapter: the eager-preview (`DriveSource::thumbnail`)
//! side of the local tdjson source (TASK-260715-3nl3mu; POL-2, POL-4,
//! SYNC-001, PLAT-AND-004).
//!
//! # Where it sits
//!
//! POL-2 makes thumbnails always eager — small, and worth having up front
//! because they make browsing fast. This module serves that preview for one
//! item, and it is deliberately *not* a small version of the ranged fetch
//! ([`crate::download`]): a thumbnail is a preview of the bytes, never the
//! bytes, so it must never hydrate the full media. It answers
//! [`DriveSource::thumbnail`](gramdrive_source::DriveSource::thumbnail)'s
//! exact contract — `Ok(None)` for "this item has no thumbnail" (a normal
//! answer, not an error) and [`SourceError::Restricted`] for protected
//! content (POL-4) — so the full `DriveSource` adapter delegates to
//! [`TdThumbnailer::thumbnail`] unchanged when the enumeration side lands.
//!
//! # Two preview sources, never the media
//!
//! [`crate::message`] already captured, for a fetchable attachment, both
//! preview flavors Telegram delivers:
//!
//! - a downloadable [`ThumbnailDescriptor`] — a *separate, small* TDLib file
//!   (a photo's smallest stored size, or a video/document's `thumbnail`
//!   member), named by its own `file_id`. Downloading it hydrates the
//!   preview, never the full-resolution media, which lives under a different
//!   `file_id` entirely (the checklist's "distinct from full-content
//!   hydration");
//! - an inline [`Minithumbnail`] — a tiny blurred JPEG delivered *inside*
//!   the message, so it costs no network at all.
//!
//! The machine prefers the downloadable preview (the checklist's "via TDLib
//! thumbnail files"), uses the inline blur as a zero-network fallback when
//! there is no usable downloadable preview, and — because the inline blur is
//! already tiny — serves it directly when the requested box is small enough
//! that it already fills it, sparing the round trip (POL-2: eager, small,
//! bounded).
//!
//! # POL-4 is the first gate
//!
//! A restricted or view-once attachment is refused as a thumbnail exactly as
//! it is refused as content — a thumbnail is a rendering of the bytes, not a
//! laxer, metadata-shaped question ([`gramdrive_testkit`]'s conformance
//! knocks on both doors). The refusal is [`SourceError::Restricted`] and it
//! costs zero requests: the normalizer already dropped every preview locator
//! for a non-fetchable attachment (fail-closed), and this gate never reaches
//! the network.
//!
//! # Bounded (AC: "bounded … never force full media download unintentionally")
//!
//! Preview downloads are whole-file synchronous downloads of a small file,
//! but the catalog is the one that maps an item to a *preview* `file_id`, and
//! a bug there must not turn a thumbnail request into a multi-gigabyte media
//! download. So [`ThumbnailConfig::max_preview_bytes`] caps it twice: a
//! preview whose size the catalog already knows to exceed the cap is skipped
//! before any request (the inline blur, or `None`, answers instead), and a
//! download whose *response* reports a size past the cap is refused rather
//! than read. The cap is defense in depth, set well above any real preview;
//! in normal operation it never trips.
//!
//! # Shape: a sans-IO machine and a thin driver
//!
//! [`ThumbnailMachine`] holds every decision — the POL-4 gate, preview
//! selection, base64 decoding of the inline blur, the download request, the
//! response validation, the byte-cap — and performs no I/O, following the
//! crate's machine convention ([`crate::download`], [`crate::history`]).
//! [`TdThumbnailer`] is the composing driver: it resolves the item through a
//! [`ThumbnailCatalog`], submits the machine's request on a [`TdClient`],
//! reads TDLib's local preview file read-only, and returns the finished
//! [`Thumbnail`]. Base64 decoding is a small in-crate routine, so serving the
//! inline blur adds no dependency (the normalizer deliberately kept it a
//! string, [`Minithumbnail::data_base64`]).
//!
//! # Reference refresh (a deliberate boundary)
//!
//! A preview `file_id`'s Telegram reference can expire like any other
//! (`FILE_REFERENCE_*`). Unlike the ranged fetch, this adapter does *not*
//! run an in-adapter `getMessage` refresh for it: a thumbnail is a secondary,
//! best-effort preview whose reference rides on the owning message's own
//! refresh path (the ranged fetch, the live-update refresh). A stale preview
//! reference therefore surfaces as [`SourceError::StaleReference`] — the
//! retryable-after-refresh class — and the caller retries once the message's
//! locators are re-learned and the catalog re-resolves. Keeping the refresh
//! protocol in one place ([`crate::download`]) keeps this module the size its
//! job warrants.
//!
//! # Temporary-file ownership and cancellation
//!
//! Every path TDLib reports is TDLib's property: this module opens it
//! read-only and never moves, renames, or deletes it (the ownership rule of
//! [`crate::download`]). Dropping the future while the preview is downloading
//! fires `cancelDownloadFile` so TDLib stops network work; concurrent
//! thumbnail requests for one preview file serialize on the same per-`file_id`
//! discipline the ranged download uses.

use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use gramdrive_model::identity::ItemId;
use gramdrive_source::{SourceError, Thumbnail, ThumbnailSpec};

use crate::download::{
    CancelGuard, DownloadPriority, FileLocks, classify_runtime_error, is_stale_reference,
    read_exact_at,
};
use crate::error::TdError;
use crate::message::{
    AttachmentAvailability, AttachmentDescriptor, Minithumbnail, ThumbnailDescriptor,
    ThumbnailFormat,
};
use crate::runtime::TdClient;

/// Default preview byte cap: 4 MiB. Far above any real Telegram thumbnail —
/// TDLib caps preview dimensions small — so it only ever trips on a
/// mis-projected `file_id`, which is exactly what it exists to catch.
const DEFAULT_MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The catalog seam
// ---------------------------------------------------------------------------

/// The per-item preview facts a thumbnail request needs, resolved by the
/// composing caller's metadata projection (the state layer, in the full
/// adapter). Built from an [`AttachmentDescriptor`] via
/// [`ThumbnailTarget::from_descriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailTarget {
    /// POL-4 availability. Anything but [`AttachmentAvailability::Fetchable`]
    /// is refused as [`SourceError::Restricted`] before any network call.
    pub availability: AttachmentAvailability,
    /// A downloadable preview the source holds a locator for — a small,
    /// dedicated TDLib thumbnail file, never the full media. `None` when the
    /// attachment carries no downloadable preview.
    pub downloadable: Option<ThumbnailDescriptor>,
    /// The inline blurred preview delivered with the message — a zero-network
    /// fallback (and, for a small enough request, the whole answer). `None`
    /// when the message carried no minithumbnail.
    pub inline: Option<Minithumbnail>,
}

impl ThumbnailTarget {
    /// Project a normalized attachment descriptor into the preview facts the
    /// thumbnail source serves.
    ///
    /// The descriptor already carries the POL-4 availability and — only for a
    /// fetchable attachment — its previews (the normalizer fails closed), so
    /// this is a total, lossless projection: it never has to re-derive
    /// protection or manufacture a preview. It is the single place the
    /// attachment mapping ([`crate::attachment`]) meets the thumbnail source.
    pub fn from_descriptor(descriptor: &AttachmentDescriptor) -> ThumbnailTarget {
        ThumbnailTarget {
            availability: descriptor.availability,
            downloadable: descriptor.thumbnail.clone(),
            inline: descriptor.minithumbnail.clone(),
        }
    }
}

/// Resolution of item identity to preview facts — the seam between this
/// adapter and the metadata store it must not own. `None` means the item has
/// no thumbnail to serve: it does not exist, is a directory, or is a file
/// with no preview. All three answer `Ok(None)`, which the contract calls a
/// normal answer; only a *restricted* attachment is an error, and that is
/// carried inside a resolved [`ThumbnailTarget`], not by returning `None`.
///
/// Implementations answer from local state and must return promptly.
pub trait ThumbnailCatalog: Send + Sync {
    /// The preview facts for `item`, or `None` when it has no thumbnail.
    fn resolve(&self, item: &ItemId) -> Option<ThumbnailTarget>;
}

/// Thumbnail adapter tuning. Policy, not durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailConfig {
    /// TDLib priority passed through to the preview `downloadFile` (1..=32).
    pub priority: DownloadPriority,
    /// Upper bound on a served preview's bytes: a backstop against a
    /// mis-projected `file_id` turning a thumbnail request into a full-media
    /// download (module docs). Set well above any real preview.
    pub max_preview_bytes: NonZeroU64,
}

impl Default for ThumbnailConfig {
    fn default() -> ThumbnailConfig {
        ThumbnailConfig {
            priority: DownloadPriority::default(),
            max_preview_bytes: NonZeroU64::new(DEFAULT_MAX_PREVIEW_BYTES)
                .unwrap_or(NonZeroU64::MIN),
        }
    }
}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// The caller's current obligation, from [`ThumbnailMachine::next_step`].
/// Idempotent: without an intervening `on_*` call the same obligation is
/// returned again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbnailStep {
    /// Submit this request on the account's client and feed the outcome to
    /// [`ThumbnailMachine::on_response`].
    Submit {
        /// The serialized-ready `downloadFile` request for the preview.
        payload: Value,
    },
    /// Read exactly `len` bytes from the start of TDLib's local preview file
    /// and feed them to [`ThumbnailMachine::on_read`] (or the failure to
    /// [`ThumbnailMachine::on_read_error`]). The file is TDLib's: open it
    /// read-only, never move or delete it (module docs).
    ReadLocal {
        /// TDLib's reported local path.
        path: String,
        /// Bytes to read — the whole preview file; never zero.
        len: NonZeroU64,
    },
    /// The request is settled: here is the answer. `Some` is the finished
    /// thumbnail; `None` is "this item has no thumbnail". Terminal.
    Answer(Option<Thumbnail>),
}

/// A usable downloadable preview: a small TDLib file and the MIME its format
/// maps to. Copy — the machine reads it across a phase transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Preview {
    /// TDLib's file id for the preview file.
    file_id: i32,
    /// The MIME the served bytes carry, from the preview's format.
    mime: &'static str,
}

/// Which stage of the request the machine is in.
#[derive(Debug)]
enum Phase {
    /// A ready answer needing no network — an inline preview, `None`, or the
    /// finished download. Terminal; repeats.
    Answer(Option<Thumbnail>),
    /// A preview download is owed; awaiting its response.
    Download(Preview),
    /// The download resolved; read the whole preview file, then form a
    /// thumbnail of `mime`.
    Read {
        mime: &'static str,
        path: String,
        len: NonZeroU64,
    },
}

/// The deterministic sans-IO thumbnail machine for one request. The driver
/// owns the wiring — see [`TdThumbnailer`] and the module docs.
#[derive(Debug)]
pub struct ThumbnailMachine {
    phase: Phase,
    cap: u64,
    priority: DownloadPriority,
    /// A `Submit` obligation is unanswered.
    outstanding: bool,
    failed: Option<SourceError>,
}

impl ThumbnailMachine {
    /// A machine for one thumbnail request, with the plan already evaluated
    /// against the resolved catalog `entry` and the requested `spec`: POL-4
    /// first, then preview selection. A refusal or a ready inline answer
    /// surfaces from the first [`next_step`](Self::next_step); a restricted
    /// attachment reaches no network (POL-4).
    pub fn new(
        entry: Option<ThumbnailTarget>,
        spec: ThumbnailSpec,
        config: &ThumbnailConfig,
    ) -> ThumbnailMachine {
        let cap = config.max_preview_bytes.get();
        let mut machine = ThumbnailMachine {
            phase: Phase::Answer(None),
            cap,
            priority: config.priority,
            outstanding: false,
            failed: None,
        };
        match plan(entry, spec, cap) {
            Ok(phase) => machine.phase = phase,
            Err(error) => machine.failed = Some(error),
        }
        machine
    }

    /// The preview file this request downloads, when one is planned — what
    /// the driver serializes concurrent requests on. `None` for an inline,
    /// absent, or restricted answer (nothing to lock, nothing to download).
    pub fn file_id(&self) -> Option<i32> {
        match &self.phase {
            Phase::Download(preview) => Some(preview.file_id),
            Phase::Answer(_) | Phase::Read { .. } => None,
        }
    }

    /// The `cancelDownloadFile` request that stops this request's network
    /// work, for the driver's abandon path. `None` unless a preview download
    /// is in flight.
    pub fn cancel_request(&self) -> Option<Value> {
        match &self.phase {
            Phase::Download(preview) => Some(json!({
                "@type": "cancelDownloadFile",
                "file_id": preview.file_id,
                "only_if_pending": false,
            })),
            Phase::Answer(_) | Phase::Read { .. } => None,
        }
    }

    /// The caller's current obligation. A terminal failure repeats.
    pub fn next_step(&mut self) -> Result<ThumbnailStep, SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        match &self.phase {
            Phase::Answer(answer) => Ok(ThumbnailStep::Answer(answer.clone())),
            Phase::Download(preview) => {
                self.outstanding = true;
                Ok(ThumbnailStep::Submit {
                    payload: json!({
                        "@type": "downloadFile",
                        "file_id": preview.file_id,
                        "priority": self.priority.get(),
                        "offset": 0,
                        // A whole-file synchronous download: `limit` 0 is
                        // TDLib's "to the end of the file". The file is a
                        // preview, small by construction and capped
                        // (module docs).
                        "limit": 0,
                        "synchronous": true,
                    }),
                })
            }
            Phase::Read { path, len, .. } => Ok(ThumbnailStep::ReadLocal {
                path: path.clone(),
                len: *len,
            }),
        }
    }

    /// Feed the outcome of the preview `downloadFile`. `Err` is the
    /// classified terminal failure of the request.
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
        let Phase::Download(preview) = &self.phase else {
            return Err(self.fail(SourceError::Internal {
                detail: "a response was fed outside the download phase".to_owned(),
            }));
        };
        let preview = *preview;
        match outcome {
            Ok(file) => match self.validate_download(&file, preview) {
                Ok(Some((path, len))) => {
                    self.phase = Phase::Read {
                        mime: preview.mime,
                        path,
                        len,
                    };
                    Ok(())
                }
                // A completed download with no bytes is no usable preview:
                // the honest answer is "no thumbnail", not an error that
                // would interrupt a browse (POL-2).
                Ok(None) => {
                    self.phase = Phase::Answer(None);
                    Ok(())
                }
                Err(error) => Err(self.fail(error)),
            },
            Err(error) => {
                let classified = if is_stale_reference(&error) {
                    // A secondary preview's reference rides on the owning
                    // message's refresh path (module docs); surface the
                    // retryable class rather than refresh here.
                    SourceError::StaleReference {
                        detail: format!("the preview's content reference expired: {error}"),
                    }
                } else {
                    classify_runtime_error(error, "downloadFile (thumbnail)")
                };
                Err(self.fail(classified))
            }
        }
    }

    /// Feed the whole preview file just read. Exactly the bytes the last
    /// [`ThumbnailStep::ReadLocal`] asked for; the machine forms the finished
    /// thumbnail from them.
    pub fn on_read(&mut self, bytes: &[u8]) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let Phase::Read { mime, len, .. } = &self.phase else {
            return Err(self.fail(SourceError::Internal {
                detail: "a local read was fed outside the read phase".to_owned(),
            }));
        };
        let mime = *mime;
        if bytes.len() as u64 != len.get() {
            // The local file held less than the download response promised —
            // a cache eviction or an external move. Retryable: the next
            // attempt re-downloads and re-reads fresh state.
            return Err(self.fail(SourceError::Unavailable {
                detail: format!(
                    "TDLib's local preview file served {} of {} bytes",
                    bytes.len(),
                    len.get()
                ),
            }));
        }
        match Thumbnail::new(mime, bytes.to_vec()) {
            Ok(thumbnail) => {
                self.phase = Phase::Answer(Some(thumbnail));
                Ok(())
            }
            // Non-empty by the length check above; fail closed rather than
            // panic (NFR-030).
            Err(invalid) => Err(self.fail(SourceError::Internal {
                detail: format!("the preview bytes did not form a thumbnail: {invalid}"),
            })),
        }
    }

    /// Feed a failed local read. Always terminal for this attempt; the
    /// classification is retryable, because a fresh attempt re-asks TDLib for
    /// current local state.
    pub fn on_read_error(&mut self, detail: String) -> Result<(), SourceError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        Err(self.fail(SourceError::Unavailable {
            detail: format!("reading TDLib's local preview file failed: {detail}"),
        }))
    }

    // -- internals ----------------------------------------------------------

    fn fail(&mut self, error: SourceError) -> SourceError {
        self.failed = Some(error.clone());
        error
    }

    /// Validate the synchronous preview `downloadFile` answer: the right
    /// file, whole-file coverage, non-empty bytes within the cap, and a local
    /// path. `Ok(None)` is a completed-but-empty preview — no thumbnail.
    fn validate_download(
        &self,
        file: &Value,
        preview: Preview,
    ) -> Result<Option<(String, NonZeroU64)>, SourceError> {
        if file.get("@type").and_then(Value::as_str) != Some("file")
            || file.get("id").and_then(Value::as_i64) != Some(i64::from(preview.file_id))
        {
            return Err(SourceError::Internal {
                detail: format!(
                    "downloadFile answered something other than preview file {}",
                    preview.file_id
                ),
            });
        }
        let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
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
        // The whole file must be local: the completion flag, or a prefix from
        // the start that reaches the known extent.
        let covered = completed || (size > 0 && offset == 0 && prefix >= size);
        if !covered {
            return Err(SourceError::Unavailable {
                detail: format!(
                    "synchronous preview download resolved without the whole file: \
                     size {size}, offset {offset}, prefix {prefix}, completed {completed}"
                ),
            });
        }
        if size == 0 {
            return Ok(None);
        }
        if size > self.cap {
            // Bounded: a preview past the cap is a mis-projected file id, not
            // a thumbnail. Refuse rather than read a full-media file.
            return Err(SourceError::Internal {
                detail: format!(
                    "the preview file is {size} bytes, past the {}-byte bound; \
                     refusing to read (a thumbnail must never hydrate full media)",
                    self.cap
                ),
            });
        }
        match local.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => {
                let len = NonZeroU64::new(size).ok_or_else(|| SourceError::Internal {
                    detail: "a positive size was not non-zero".to_owned(),
                })?;
                Ok(Some((path.to_owned(), len)))
            }
            _ => Err(SourceError::Unavailable {
                detail: "the preview download completed but TDLib reported no local path"
                    .to_owned(),
            }),
        }
    }
}

/// Evaluate the plan for one request: POL-4 first, then preview selection.
/// `Err` is the POL-4 refusal; `Ok` is the terminal answer or the download to
/// run. Pure — the whole decision is testable without I/O.
fn plan(
    entry: Option<ThumbnailTarget>,
    spec: ThumbnailSpec,
    cap: u64,
) -> Result<Phase, SourceError> {
    // No such item, a directory, or a file with no preview: a normal
    // "no thumbnail" answer, not an error (the contract's `Ok(None)`).
    let Some(target) = entry else {
        return Ok(Phase::Answer(None));
    };
    // POL-4: a restricted or view-once attachment is refused as a thumbnail
    // exactly as it is refused as content, and costs zero requests.
    if target.availability != AttachmentAvailability::Fetchable {
        return Err(SourceError::Restricted {
            detail: match target.availability {
                AttachmentAvailability::Restricted => {
                    "the attachment is save-restricted (POL-4); its preview is never fetched"
                }
                AttachmentAvailability::ViewOnce => {
                    "the attachment is view-once (POL-4); its preview is never persisted"
                }
                AttachmentAvailability::Fetchable => "unreachable",
            }
            .to_owned(),
        });
    }
    let inline = target.inline.as_ref();
    let inline_thumbnail = inline.and_then(decode_minithumbnail);
    let downloadable = target
        .downloadable
        .as_ref()
        .and_then(|descriptor| usable_preview(descriptor, cap));
    // The inline blur already fills the requested box: serve it and skip the
    // network entirely (POL-2: eager, small, bounded).
    if let (Some(minithumbnail), Some(thumbnail)) = (inline, &inline_thumbnail)
        && inline_covers(spec, minithumbnail)
    {
        return Ok(Phase::Answer(Some(thumbnail.clone())));
    }
    // Otherwise prefer the real downloadable preview; fall back to the inline
    // blur; then to "no thumbnail".
    if let Some(preview) = downloadable {
        return Ok(Phase::Download(preview));
    }
    Ok(Phase::Answer(inline_thumbnail))
}

/// Whether the inline blur's own dimensions already cover the requested box,
/// so no larger preview is worth a round trip. Unknown dimensions never
/// cover — a preview of unknown size is not assumed large enough.
fn inline_covers(spec: ThumbnailSpec, minithumbnail: &Minithumbnail) -> bool {
    match (minithumbnail.width, minithumbnail.height) {
        (Some(width), Some(height)) => {
            width >= spec.max_width_px.get() && height >= spec.max_height_px.get()
        }
        _ => false,
    }
}

/// A downloadable preview reduced to what the machine needs, or `None` when
/// it is not worth downloading: an undecodable format (no MIME to label the
/// bytes), an empty preview, or one whose known size already exceeds the cap.
/// A `None` here falls back to the inline blur, not to an error.
fn usable_preview(descriptor: &ThumbnailDescriptor, cap: u64) -> Option<Preview> {
    let mime = mime_for(&descriptor.format)?;
    if let Some(size) = descriptor.size
        && (size == 0 || size > cap)
    {
        return None;
    }
    Some(Preview {
        file_id: descriptor.file_id,
        mime,
    })
}

/// The MIME a preview format's bytes carry, or `None` for a format this build
/// cannot label (an undecodable preview is not served — the normalizer's
/// [`ThumbnailFormat::Unknown`] leaves the choice here).
fn mime_for(format: &ThumbnailFormat) -> Option<&'static str> {
    Some(match format {
        ThumbnailFormat::Jpeg => "image/jpeg",
        ThumbnailFormat::Png => "image/png",
        ThumbnailFormat::Webp => "image/webp",
        ThumbnailFormat::Gif => "image/gif",
        ThumbnailFormat::Mpeg4 => "video/mp4",
        ThumbnailFormat::Webm => "video/webm",
        ThumbnailFormat::Tgs => "application/x-tgsticker",
        ThumbnailFormat::Unknown { .. } => return None,
    })
}

/// Decode one inline minithumbnail into a finished JPEG thumbnail. TDLib
/// minithumbnails are JPEG, encoded base64 as tdjson delivers bytes; an
/// undecodable or empty payload degrades to `None` rather than a broken
/// preview.
fn decode_minithumbnail(minithumbnail: &Minithumbnail) -> Option<Thumbnail> {
    let bytes = decode_base64(&minithumbnail.data_base64)?;
    Thumbnail::new("image/jpeg", bytes).ok()
}

/// Decode standard (RFC 4648) base64, tolerating both padded and unpadded
/// input. `None` on any invalid character or an impossible length. Kept
/// in-crate so serving the inline blur pulls in no base64 dependency
/// (module docs).
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u8> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut values = Vec::with_capacity(input.len());
    for &byte in input.as_bytes() {
        // Padding: valid only as a trailing run, so stop here — the length of
        // what precedes it fixes the byte count.
        if byte == b'=' {
            break;
        }
        values.push(sextet(byte)?);
    }
    // A single trailing sextet cannot exist: base64 emits 2, 3, or 4.
    if values.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(values.len() / 4 * 3 + 2);
    let mut quads = values.chunks_exact(4);
    for quad in &mut quads {
        out.push((quad[0] << 2) | (quad[1] >> 4));
        out.push((quad[1] << 4) | (quad[2] >> 2));
        out.push((quad[2] << 6) | quad[3]);
    }
    let tail = quads.remainder();
    match tail.len() {
        0 => {}
        2 => out.push((tail[0] << 2) | (tail[1] >> 4)),
        3 => {
            out.push((tail[0] << 2) | (tail[1] >> 4));
            out.push((tail[1] << 4) | (tail[2] >> 2));
        }
        // Unreachable: the `% 4 == 1` guard rejected a 1-sextet tail.
        _ => return None,
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// The thumbnail driver: `DriveSource::thumbnail`'s implementation for the
/// tdjson source, shaped for the full adapter to delegate to (module docs).
pub struct TdThumbnailer {
    client: TdClient,
    catalog: Arc<dyn ThumbnailCatalog>,
    config: ThumbnailConfig,
    locks: FileLocks,
}

impl std::fmt::Debug for TdThumbnailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdThumbnailer")
            .field("client", &self.client)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TdThumbnailer {
    /// A thumbnailer submitting on `client` and resolving items through
    /// `catalog`.
    pub fn new(
        client: TdClient,
        catalog: Arc<dyn ThumbnailCatalog>,
        config: ThumbnailConfig,
    ) -> TdThumbnailer {
        TdThumbnailer {
            client,
            catalog,
            config,
            locks: FileLocks::default(),
        }
    }

    /// A thumbnail for `item` fitting within `spec`, when the source has one
    /// — `DriveSource::thumbnail`'s contract (`gramdrive_source::source`),
    /// implemented over TDLib previews. `Ok(None)` is "no thumbnail";
    /// restricted content is [`SourceError::Restricted`] (POL-4).
    pub fn thumbnail<'a>(
        &'a self,
        item: ItemId,
        spec: ThumbnailSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Thumbnail>, SourceError>> + Send + 'a>> {
        Box::pin(async move { self.thumbnail_inner(item, spec).await })
    }

    async fn thumbnail_inner(
        &self,
        item: ItemId,
        spec: ThumbnailSpec,
    ) -> Result<Option<Thumbnail>, SourceError> {
        let entry = self.catalog.resolve(&item);
        let mut machine = ThumbnailMachine::new(entry, spec, &self.config);
        // A refusal, an inline answer, and "no thumbnail" all cost no lock
        // and no network; only a live download serializes on its preview file
        // (one download conversation per file, as the ranged fetch does).
        let _serialized = match machine.file_id() {
            Some(file_id) => Some(self.locks.acquire(file_id).await),
            None => None,
        };
        let mut cancel = CancelGuard::disarmed(self.client.clone());
        loop {
            match machine.next_step()? {
                ThumbnailStep::Submit { payload } => {
                    cancel.arm(machine.cancel_request());
                    let outcome = match self.client.request(payload) {
                        Ok(pending) => pending.await,
                        Err(error) => Err(error),
                    };
                    cancel.disarm();
                    machine.on_response(outcome)?;
                }
                ThumbnailStep::ReadLocal { path, len } => {
                    // The whole preview file, in one bounded read (it is small
                    // and capped). It is TDLib's file: read-only, in place.
                    match read_exact_at(&path, 0, len.get()) {
                        Ok(bytes) => machine.on_read(&bytes)?,
                        Err(error) => {
                            machine.on_read_error(format!("{path}: {error}"))?;
                        }
                    }
                }
                ThumbnailStep::Answer(answer) => return Ok(answer),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::num::NonZeroU32;

    const FILE_ID: i32 = 900;

    fn config() -> ThumbnailConfig {
        ThumbnailConfig {
            priority: DownloadPriority::new(5).expect("5 is in range"),
            max_preview_bytes: NonZeroU64::new(1024).expect("non-zero"),
        }
    }

    fn spec(side: u32) -> ThumbnailSpec {
        let side = NonZeroU32::new(side).expect("non-zero");
        ThumbnailSpec {
            max_width_px: side,
            max_height_px: side,
        }
    }

    fn descriptor(format: ThumbnailFormat, size: Option<u64>) -> ThumbnailDescriptor {
        ThumbnailDescriptor {
            format,
            file_id: FILE_ID,
            remote_id: Some("r".to_owned()),
            remote_unique_id: Some("u".to_owned()),
            size,
            width: Some(320),
            height: Some(240),
        }
    }

    fn minithumbnail(width: u32, height: u32, data_base64: &str) -> Minithumbnail {
        Minithumbnail {
            width: Some(width),
            height: Some(height),
            data_base64: data_base64.to_owned(),
        }
    }

    fn fetchable(
        downloadable: Option<ThumbnailDescriptor>,
        inline: Option<Minithumbnail>,
    ) -> ThumbnailTarget {
        ThumbnailTarget {
            availability: AttachmentAvailability::Fetchable,
            downloadable,
            inline,
        }
    }

    fn file_response(size: u64, completed: bool, path: &str) -> Value {
        json!({
            "@type": "file",
            "id": FILE_ID,
            "size": size,
            "local": {
                "@type": "localFile",
                "path": path,
                "download_offset": 0,
                "downloaded_prefix_size": size,
                "is_downloading_active": false,
                "is_downloading_completed": completed,
            },
        })
    }

    // -- base64 -------------------------------------------------------------

    #[test]
    fn base64_round_trips_known_vectors() {
        // RFC 4648 test vectors, padded and unpadded.
        assert_eq!(decode_base64("").as_deref(), Some(&b""[..]));
        assert_eq!(decode_base64("Zg==").as_deref(), Some(&b"f"[..]));
        assert_eq!(decode_base64("Zm8=").as_deref(), Some(&b"fo"[..]));
        assert_eq!(decode_base64("Zm9v").as_deref(), Some(&b"foo"[..]));
        assert_eq!(decode_base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        // Unpadded forms decode identically.
        assert_eq!(decode_base64("Zg").as_deref(), Some(&b"f"[..]));
        assert_eq!(decode_base64("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        // Every byte value survives the alphabet's edges (+ and /).
        assert_eq!(decode_base64("+/8=").as_deref(), Some(&[0xfb, 0xff][..]));
    }

    #[test]
    fn base64_rejects_invalid_input() {
        // A stray non-alphabet character.
        assert_eq!(decode_base64("Zm8*"), None);
        // A 1-sextet tail is impossible base64.
        assert_eq!(decode_base64("Zm8vZ"), None);
        // A space is not in the alphabet (no whitespace tolerance).
        assert_eq!(decode_base64("Zm 8"), None);
    }

    // -- MIME mapping -------------------------------------------------------

    #[test]
    fn every_known_format_maps_to_a_mime_and_unknown_does_not() {
        assert_eq!(mime_for(&ThumbnailFormat::Jpeg), Some("image/jpeg"));
        assert_eq!(mime_for(&ThumbnailFormat::Png), Some("image/png"));
        assert_eq!(mime_for(&ThumbnailFormat::Webp), Some("image/webp"));
        assert_eq!(mime_for(&ThumbnailFormat::Gif), Some("image/gif"));
        assert_eq!(mime_for(&ThumbnailFormat::Mpeg4), Some("video/mp4"));
        assert_eq!(mime_for(&ThumbnailFormat::Webm), Some("video/webm"));
        assert_eq!(
            mime_for(&ThumbnailFormat::Tgs),
            Some("application/x-tgsticker")
        );
        assert_eq!(
            mime_for(&ThumbnailFormat::Unknown { raw_type: None }),
            None,
            "an undecodable format is not served as a thumbnail"
        );
    }

    // -- preview selection --------------------------------------------------

    #[test]
    fn no_entry_is_no_thumbnail() {
        let mut machine = ThumbnailMachine::new(None, spec(256), &config());
        assert_eq!(machine.next_step(), Ok(ThumbnailStep::Answer(None)));
        assert_eq!(machine.file_id(), None, "no thumbnail, nothing to lock");
    }

    #[test]
    fn restricted_and_view_once_are_refused_before_any_request() {
        for availability in [
            AttachmentAvailability::Restricted,
            AttachmentAvailability::ViewOnce,
        ] {
            let target = ThumbnailTarget {
                availability,
                downloadable: Some(descriptor(ThumbnailFormat::Jpeg, Some(64))),
                inline: None,
            };
            let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
            assert!(
                matches!(machine.next_step(), Err(SourceError::Restricted { .. })),
                "{availability:?} must be refused as Restricted (POL-4)"
            );
            assert_eq!(machine.file_id(), None, "POL-4 costs no lock, no request");
        }
    }

    #[test]
    fn a_fetchable_downloadable_preview_plans_a_download() {
        let target = fetchable(Some(descriptor(ThumbnailFormat::Jpeg, Some(64))), None);
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        assert_eq!(machine.file_id(), Some(FILE_ID));
        let Ok(ThumbnailStep::Submit { payload }) = machine.next_step() else {
            panic!("a downloadable preview downloads");
        };
        assert_eq!(
            payload,
            json!({
                "@type": "downloadFile",
                "file_id": FILE_ID,
                "priority": 5,
                "offset": 0,
                "limit": 0,
                "synchronous": true,
            })
        );
        // The obligation repeats until the response is fed.
        assert!(matches!(
            machine.next_step(),
            Ok(ThumbnailStep::Submit { .. })
        ));
    }

    #[test]
    fn an_unknown_format_preview_falls_back_to_no_thumbnail() {
        // No inline blur, and the only downloadable is undecodable.
        let target = fetchable(
            Some(descriptor(
                ThumbnailFormat::Unknown { raw_type: None },
                Some(64),
            )),
            None,
        );
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        assert_eq!(machine.next_step(), Ok(ThumbnailStep::Answer(None)));
        assert_eq!(machine.file_id(), None);
    }

    #[test]
    fn an_oversize_known_preview_is_skipped_for_the_inline_blur() {
        // The downloadable preview's known size is past the cap; the inline
        // blur answers instead, with no network.
        let target = fetchable(
            Some(descriptor(ThumbnailFormat::Jpeg, Some(4096))), // cap is 1024
            Some(minithumbnail(40, 30, "aGVsbG8=")),
        );
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        match machine.next_step() {
            Ok(ThumbnailStep::Answer(Some(thumbnail))) => {
                assert_eq!(thumbnail.mime_type(), "image/jpeg");
                assert_eq!(thumbnail.bytes(), b"hello");
            }
            other => panic!("expected the inline blur, got {other:?}"),
        }
        assert_eq!(
            machine.file_id(),
            None,
            "the oversize preview never downloads"
        );
    }

    #[test]
    fn the_inline_blur_answers_a_box_it_already_covers() {
        // A 16px box, a 40x30 blur: the blur already fills it, so no network.
        let target = fetchable(
            Some(descriptor(ThumbnailFormat::Jpeg, Some(64))),
            Some(minithumbnail(40, 30, "aGVsbG8=")),
        );
        let mut machine = ThumbnailMachine::new(Some(target), spec(16), &config());
        match machine.next_step() {
            Ok(ThumbnailStep::Answer(Some(thumbnail))) => {
                assert_eq!(thumbnail.bytes(), b"hello");
            }
            other => panic!("expected the inline blur, got {other:?}"),
        }
        assert_eq!(machine.file_id(), None);
    }

    #[test]
    fn a_large_box_prefers_the_downloadable_over_the_inline_blur() {
        // The same blur does not cover a 256px box, so the real preview wins.
        let target = fetchable(
            Some(descriptor(ThumbnailFormat::Jpeg, Some(64))),
            Some(minithumbnail(40, 30, "aGVsbG8=")),
        );
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        assert_eq!(machine.file_id(), Some(FILE_ID));
        assert!(matches!(
            machine.next_step(),
            Ok(ThumbnailStep::Submit { .. })
        ));
    }

    #[test]
    fn only_an_inline_blur_answers_without_a_download() {
        let target = fetchable(None, Some(minithumbnail(40, 30, "aGVsbG8=")));
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        match machine.next_step() {
            Ok(ThumbnailStep::Answer(Some(thumbnail))) => {
                assert_eq!(thumbnail.bytes(), b"hello");
            }
            other => panic!("expected the inline fallback, got {other:?}"),
        }
    }

    #[test]
    fn a_fetchable_attachment_with_no_preview_is_no_thumbnail() {
        let mut machine = ThumbnailMachine::new(Some(fetchable(None, None)), spec(256), &config());
        assert_eq!(machine.next_step(), Ok(ThumbnailStep::Answer(None)));
    }

    #[test]
    fn from_descriptor_projects_availability_and_previews() {
        let attachment = AttachmentDescriptor {
            kind: crate::message::AttachmentKind::Video,
            file_id: 1,
            remote_id: None,
            remote_unique_id: None,
            file_name: None,
            mime_type: Some("video/mp4".to_owned()),
            size: Some(1_000),
            width: Some(1280),
            height: Some(720),
            duration_secs: Some(10),
            thumbnail: Some(descriptor(ThumbnailFormat::Jpeg, Some(64))),
            minithumbnail: Some(minithumbnail(40, 30, "aGVsbG8=")),
            availability: AttachmentAvailability::Fetchable,
        };
        let target = ThumbnailTarget::from_descriptor(&attachment);
        assert_eq!(target.availability, AttachmentAvailability::Fetchable);
        assert_eq!(
            target.downloadable.as_ref().map(|d| d.file_id),
            Some(FILE_ID)
        );
        assert_eq!(
            target.inline.as_ref().map(|m| m.data_base64.as_str()),
            Some("aGVsbG8=")
        );
    }

    // -- download response validation ---------------------------------------

    fn download_machine() -> ThumbnailMachine {
        let target = fetchable(Some(descriptor(ThumbnailFormat::Jpeg, None)), None);
        ThumbnailMachine::new(Some(target), spec(256), &config())
    }

    #[test]
    fn a_covering_response_moves_to_a_whole_file_read() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        machine
            .on_response(Ok(file_response(64, true, "/td/thumb.jpg")))
            .expect("the response validates");
        assert_eq!(
            machine.next_step(),
            Ok(ThumbnailStep::ReadLocal {
                path: "/td/thumb.jpg".to_owned(),
                len: NonZeroU64::new(64).expect("non-zero"),
            })
        );
    }

    #[test]
    fn a_completed_empty_preview_is_no_thumbnail_not_an_error() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        machine
            .on_response(Ok(file_response(0, true, "/td/thumb.jpg")))
            .expect("an empty preview is a normal answer");
        assert_eq!(machine.next_step(), Ok(ThumbnailStep::Answer(None)));
    }

    #[test]
    fn a_non_covering_response_is_unavailable() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        let response = json!({
            "@type": "file",
            "id": FILE_ID,
            "size": 64,
            "local": {
                "@type": "localFile",
                "path": "/td/thumb.jpg",
                "download_offset": 0,
                "downloaded_prefix_size": 32,
                "is_downloading_completed": false,
            },
        });
        let error = machine
            .on_response(Ok(response))
            .expect_err("half a preview is not coverage");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_preview_past_the_cap_is_refused_to_never_hydrate_media() {
        let mut machine = download_machine(); // cap 1024
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Ok(file_response(4096, true, "/td/huge.bin")))
            .expect_err("a 4 KiB 'preview' past a 1 KiB cap is refused");
        assert!(
            matches!(error, SourceError::Internal { .. }),
            "a preview past the bound is a mis-projection, refused not read"
        );
    }

    #[test]
    fn a_response_for_the_wrong_file_is_internal() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        let mut response = file_response(64, true, "/td/thumb.jpg");
        response["id"] = json!(FILE_ID + 1);
        let error = machine
            .on_response(Ok(response))
            .expect_err("the wrong file cannot serve this preview");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    #[test]
    fn a_response_without_a_path_is_unavailable() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Ok(file_response(64, true, "")))
            .expect_err("no path, no preview");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    // -- error classification -----------------------------------------------

    #[test]
    fn a_stale_preview_reference_surfaces_as_stale_reference() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Err(TdError::Td {
                code: 400,
                message: "FILE_REFERENCE_EXPIRED".to_owned(),
            }))
            .expect_err("an expired reference is retryable-after-refresh");
        assert!(
            matches!(error, SourceError::StaleReference { .. }),
            "a thumbnail's stale reference is StaleReference, not a refresh here"
        );
    }

    #[test]
    fn a_flood_wait_carries_its_stated_delay() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        let error = machine
            .on_response(Err(TdError::Td {
                code: 429,
                message: "Too Many Requests: retry after 7".to_owned(),
            }))
            .expect_err("flood");
        match error {
            SourceError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(7)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn a_gate_failure_repeats_and_never_recovers() {
        let target = ThumbnailTarget {
            availability: AttachmentAvailability::Restricted,
            downloadable: None,
            inline: None,
        };
        let mut machine = ThumbnailMachine::new(Some(target), spec(256), &config());
        let first = machine.next_step().expect_err("restricted");
        let second = machine.next_step().expect_err("still restricted");
        assert_eq!(first, second);
    }

    // -- read accounting ----------------------------------------------------

    fn into_read_phase(machine: &mut ThumbnailMachine) {
        let _ = machine.next_step().expect("submit");
        machine
            .on_response(Ok(file_response(5, true, "/td/thumb.jpg")))
            .expect("validates");
        let _ = machine.next_step().expect("read obligation");
    }

    #[test]
    fn the_read_bytes_become_the_thumbnail() {
        let mut machine = download_machine();
        into_read_phase(&mut machine);
        machine.on_read(b"hello").expect("the preview accounts");
        match machine.next_step() {
            Ok(ThumbnailStep::Answer(Some(thumbnail))) => {
                assert_eq!(thumbnail.mime_type(), "image/jpeg");
                assert_eq!(thumbnail.bytes(), b"hello");
            }
            other => panic!("expected the finished thumbnail, got {other:?}"),
        }
    }

    #[test]
    fn a_short_read_is_unavailable_not_silent() {
        let mut machine = download_machine();
        into_read_phase(&mut machine);
        let error = machine
            .on_read(b"hi")
            .expect_err("2 of 5 bytes is a truncated preview");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_read_error_is_unavailable() {
        let mut machine = download_machine();
        into_read_phase(&mut machine);
        let error = machine
            .on_read_error("permission denied".to_owned())
            .expect_err("a failed read fails the attempt");
        assert!(matches!(error, SourceError::Unavailable { .. }));
    }

    #[test]
    fn a_response_with_nothing_outstanding_is_internal() {
        let mut machine = download_machine();
        let error = machine
            .on_response(Ok(json!({"@type": "ok"})))
            .expect_err("nothing was submitted");
        assert!(matches!(error, SourceError::Internal { .. }));
    }

    #[test]
    fn the_cancel_request_names_the_preview_file() {
        let mut machine = download_machine();
        let _ = machine.next_step().expect("submit");
        assert_eq!(
            machine.cancel_request(),
            Some(json!({
                "@type": "cancelDownloadFile",
                "file_id": FILE_ID,
                "only_if_pending": false,
            }))
        );
    }
}
