//! The deterministic fake `DriveSource` (TASK-260715-3uft8j).
//!
//! [`FakeSource`] plays a [`SourceScript`] against the real
//! [`DriveSource`] contract: snapshot paging, a durable change feed, ranged
//! delivery into a sink, thumbnails, and the whole failure taxonomy —
//! every answer scripted, every run identical.
//!
//! # What "deterministic" buys, and what it costs
//!
//! Nothing here reads a clock, spawns a thread, or draws entropy. The three
//! things a real backend leaves to chance are each replaced by something a
//! test states outright:
//!
//! | Real backend | Here |
//! |---|---|
//! | The backend changes when it changes | [`advance`](FakeSource::advance) applies one scripted batch |
//! | Latency varies | [`Fault::delay`](crate::Fault::delay) yields a stated number of times |
//! | Chunk boundaries vary | [`ChunkPlan`](crate::ChunkPlan) draws them from the script's seed |
//!
//! The cost is that a test cannot ask this fake "what happens under load" —
//! it has no load. What it can ask is every question that actually has a
//! contractual answer, and get the same answer forever.
//!
//! # Revisions, snapshots, and why a page token names one
//!
//! A page token minted here carries the revision it was minted at, and a
//! continuation presented at a different revision is refused with
//! [`SourceError::CursorRejected`]. That is stricter than SYNC-003 requires
//! — the source could keep serving an old snapshot — and deliberately so:
//! the alternative is a fake that splices two states into one enumeration
//! and hands back a listing with a duplicate or a hole, which is precisely
//! the contract failure the conformance suite exists to catch. Refusing is
//! always contract-legal; splicing never is. The practical consequence: a
//! test that wants an uninterrupted enumeration does not call `advance`
//! mid-enumeration, and a test that wants to prove the caller re-baselines
//! correctly does exactly that.
//!
//! ```
//! # use gramdrive_testkit::{FakeSource, SourceScript, exec, fixture};
//! # use gramdrive_testkit::source::{DirectoryKind, DriveSource, PageRequest};
//! # use std::num::NonZeroU32;
//! let scope = fixture::scope();
//! let script = SourceScript::builder(scope)
//!     .item(fixture::directory(
//!         fixture::account_root_id(scope), None, "Account", "m1", DirectoryKind::Root,
//!     ).expect("valid fixture"))
//!     .item(fixture::directory(
//!         fixture::chat_id(scope, 100), Some(fixture::account_root_id(scope)),
//!         "Team", "m2", DirectoryKind::Chat,
//!     ).expect("valid fixture"))
//!     .build()
//!     .expect("valid script");
//!
//! let source = FakeSource::new(script);
//! let page = exec::drive(source.children(
//!     fixture::account_root_id(scope),
//!     PageRequest::first(NonZeroU32::new(10).expect("non-zero")),
//! )).expect("enumeration succeeds");
//!
//! assert_eq!(page.items.len(), 1);
//! assert_eq!(page.items[0].display_name, "Team");
//! assert_eq!(source.calls().len(), 1);
//! ```

use std::cmp::min;
use std::sync::{Arc, Mutex};

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountScope, ItemId};
use gramdrive_model::version::ContentVersion;
use gramdrive_source::item::FileFacts;
use gramdrive_source::{
    ChangePage, ContentAvailability, ContentChunk, ContentSink, DriveSource, FetchRequest,
    ItemContent, ItemPage, PageRequest, PageToken, SinkControl, SourceError, SourceFuture,
    SourceItem, Thumbnail, ThumbnailSpec,
};

use crate::exec::Yield;
use crate::fault::{Effect, Operation};
use crate::record::{Call, CallGuard, Interaction, Recorder, lock};
use crate::rng::{SplitMix64, fnv1a};
use crate::script::{ChunkPlan, SourceScript};
use crate::tree::Tree;

/// A page token's prefix. Tokens are opaque to the core (DEC-003) — the
/// format is this source's private business, and only this source parses it.
const PAGE_TOKEN_PREFIX: &str = "rev:";
/// Separator between the token's revision and its offset.
const PAGE_TOKEN_OFFSET: &str = "/at:";
/// A change cursor payload's prefix.
const CURSOR_PREFIX: &str = "rev:";

/// The mutable half of a playing source.
#[derive(Debug)]
struct Playback {
    revision: u32,
    /// One counter per script fault, indexed alike. Counts calls that
    /// matched the fault's operation and item filter, whether or not the
    /// occurrence fired.
    fault_counts: Vec<u32>,
}

/// A deterministic `DriveSource` playing a [`SourceScript`].
///
/// Shared like any source (`Send + Sync`, held behind `Arc`); concurrent
/// calls are safe and are recorded in call order. Two sources may play the
/// same script independently — each has its own revision and its own
/// recording.
#[derive(Debug)]
pub struct FakeSource {
    script: Arc<SourceScript>,
    playback: Mutex<Playback>,
    recorder: Recorder,
}

impl FakeSource {
    /// A source at revision 0 playing `script`.
    pub fn new(script: SourceScript) -> Self {
        Self::from_shared(Arc::new(script))
    }

    /// A source playing a script shared with other sources.
    ///
    /// Scripts are immutable, so sharing one is free and safe — useful for
    /// a test that needs two independent views of the same backend.
    pub fn from_shared(script: Arc<SourceScript>) -> Self {
        let fault_counts = vec![0; script.faults.len()];
        Self {
            script,
            playback: Mutex::new(Playback {
                revision: 0,
                fault_counts,
            }),
            recorder: Recorder::new(),
        }
    }

    /// The script being played.
    pub fn script(&self) -> &SourceScript {
        &self.script
    }

    /// The revision the source currently serves.
    pub fn revision(&self) -> u32 {
        lock(&self.playback).revision
    }

    /// Applies the next change batch, if there is one.
    ///
    /// Returns `false` when the feed is drained. This is the only thing
    /// that changes what the source serves — nothing advances on its own.
    pub fn advance(&self) -> bool {
        let mut playback = lock(&self.playback);
        if playback.revision >= self.script.batch_count() {
            return false;
        }
        playback.revision += 1;
        true
    }

    /// Advances to `revision`.
    ///
    /// Returns `false` if `revision` is behind the current one or past the
    /// last batch: a change feed moves forward, and a source cannot serve
    /// a revision its script does not describe.
    pub fn advance_to(&self, revision: u32) -> bool {
        let mut playback = lock(&self.playback);
        if revision < playback.revision || revision > self.script.batch_count() {
            return false;
        }
        playback.revision = revision;
        true
    }

    /// Applies every remaining batch and returns the revision reached.
    pub fn advance_all(&self) -> u32 {
        let mut playback = lock(&self.playback);
        playback.revision = self.script.batch_count();
        playback.revision
    }

    /// Every recorded interaction, in call order.
    ///
    /// The evidence for what the caller did: the arguments of each call and
    /// how it ended, including
    /// [`Outcome::Cancelled`](crate::Outcome::Cancelled) for a future that
    /// was dropped, with the bytes delivered before the drop.
    pub fn interactions(&self) -> Vec<Interaction> {
        self.recorder.snapshot()
    }

    /// Just the calls, in order — for asserting the request sequence
    /// without the outcomes.
    pub fn calls(&self) -> Vec<Call> {
        self.recorder
            .snapshot()
            .into_iter()
            .map(|interaction| interaction.call)
            .collect()
    }

    /// Drops every recorded interaction.
    ///
    /// For tests that set up through the source and then assert only on
    /// what follows.
    ///
    /// A call still in flight across the clear is dropped with the rest and
    /// never reappears: when it settles, it settles nothing. The alternative
    /// — letting it write into the fresh log — would attribute its outcome
    /// to whichever call inherited its position.
    pub fn clear_interactions(&self) {
        self.recorder.clear();
    }

    // --- scripted faults ---------------------------------------------------

    /// Runs the fault gate for one call: counts matches, applies the delay
    /// of the first firing fault, and reports its effect.
    ///
    /// Every matching fault's counter advances, not just the firing one, so
    /// [`Occurrence::Nth`](crate::Occurrence::Nth) always means "the n-th
    /// call to this operation for this item" regardless of what other
    /// faults the script carries. Two faults on the same operation with
    /// `Nth(1)` and `Nth(2)` therefore fire on the first and second call,
    /// which is the only reading that composes.
    async fn gate(&self, operation: Operation, item: Option<&ItemId>) -> Effect {
        let firing = {
            let mut playback = lock(&self.playback);
            let mut firing = None;
            for (index, fault) in self.script.faults.iter().enumerate() {
                if !fault.matches(operation, item) {
                    continue;
                }
                let Some(count) = playback.fault_counts.get_mut(index) else {
                    continue;
                };
                *count = count.saturating_add(1);
                let fired = fault.occurrence.fires_on(*count);
                if fired && firing.is_none() {
                    firing = Some((fault.delay_yields, fault.effect.clone()));
                }
            }
            firing
        };

        let Some((delay, effect)) = firing else {
            return Effect::Proceed;
        };
        // Outside the lock: a delay is a sequence of yields, and holding a
        // mutex across an await would serialize every concurrent call
        // behind the slow one — and stop the future being `Send`.
        Yield::new(delay).await;
        effect
    }

    // --- synchronous answer computation ------------------------------------
    //
    // Each of these takes the playback lock, computes an answer, and drops
    // it before returning. The async methods await *first* and lock after,
    // so no guard is ever alive across an await point.

    fn root_now(&self) -> Result<SourceItem, SourceError> {
        let playback = lock(&self.playback);
        let tree = self.tree_at(playback.revision)?;
        tree.get(&self.script.root).cloned().ok_or_else(|| {
            // The script validator proved a root exists at every revision.
            SourceError::Internal {
                detail: "script has no root at the current revision".to_owned(),
            }
        })
    }

    fn children_now(
        &self,
        parent: &ItemId,
        request: &PageRequest,
    ) -> Result<ItemPage, SourceError> {
        let playback = lock(&self.playback);
        let revision = playback.revision;
        let tree = self.tree_at(revision)?;

        let Some(item) = tree.get(parent) else {
            return Err(SourceError::NotFound {
                detail: format!("no item {parent} at revision {revision}"),
            });
        };
        if !item.is_directory() {
            return Err(SourceError::InvalidRequest {
                detail: format!("item {parent} is a file and has no children"),
            });
        }
        let snapshot = item.metadata_version.clone();

        let offset = match &request.continuation {
            None => 0,
            Some(token) => parse_page_token(token, revision)?,
        };
        let ids = tree.children_of(parent);
        let start = min(offset, ids.len());
        let wanted = usize::try_from(request.max_items.get()).unwrap_or(usize::MAX);
        let end = min(start.saturating_add(wanted), ids.len());

        let items = ids[start..end]
            .iter()
            .filter_map(|id| tree.get(id).cloned())
            .collect::<Vec<_>>();
        let next = if end < ids.len() {
            Some(mint_page_token(revision, end)?)
        } else {
            None
        };

        Ok(ItemPage {
            snapshot,
            items,
            next,
        })
    }

    fn latest_cursor_now(&self) -> Result<ChangeCursor, SourceError> {
        let revision = lock(&self.playback).revision;
        self.mint_cursor(revision)
    }

    fn changes_now(&self, cursor: &ChangeCursor) -> Result<ChangePage, SourceError> {
        let revision = lock(&self.playback).revision;
        let position = self.parse_cursor(cursor, revision)?;

        if position >= revision {
            // Drained: the caller is level with the source.
            return Ok(ChangePage {
                changes: Vec::new(),
                next: self.mint_cursor(position)?,
                more_available: false,
            });
        }

        let changes = self
            .script
            .batch_at(position)
            .ok_or_else(|| SourceError::Internal {
                detail: format!("script has no batch at revision {position}"),
            })?
            .to_vec();
        let next = position.saturating_add(1);

        Ok(ChangePage {
            changes,
            next: self.mint_cursor(next)?,
            more_available: next < revision,
        })
    }

    /// Resolves a fetch against the current revision, returning the bytes
    /// the whole content object holds at the pinned version.
    ///
    /// Every contractual fetch failure is decided here, before a single
    /// byte moves: missing item, directory, restricted content, a stale
    /// pin, a range past the extent.
    fn fetch_content(&self, request: &FetchRequest) -> Result<Vec<u8>, SourceError> {
        let playback = lock(&self.playback);
        let revision = playback.revision;
        let tree = self.tree_at(revision)?;

        let Some(item) = tree.get(&request.item) else {
            return Err(SourceError::NotFound {
                detail: format!("no item {} at revision {revision}", request.item),
            });
        };
        let ItemContent::File(facts) = &item.content else {
            return Err(SourceError::InvalidRequest {
                detail: format!("item {} is a directory and has no content", request.item),
            });
        };
        if facts.availability == ContentAvailability::Restricted {
            return Err(SourceError::Restricted {
                detail: format!("item {} is restricted at the source", request.item),
            });
        }
        if facts.content_version != request.version {
            // The pin is stale: the content moved on before the fetch
            // started. Same conflict the mid-flight race produces, noticed
            // earlier (SYNC-042).
            return Err(SourceError::VersionConflict {
                current: Some(facts.content_version.clone()),
                detail: format!(
                    "fetch pinned {} but the source serves {}",
                    request.version, facts.content_version
                ),
            });
        }

        let bytes = self
            .script
            .blob(&request.item, &request.version)
            .ok_or_else(|| SourceError::Internal {
                detail: format!(
                    "script registers no content for {} at {}",
                    request.item, request.version
                ),
            })?;

        let extent = bytes.len() as u64;
        if request.range.end() > extent {
            return Err(SourceError::InvalidRequest {
                detail: format!(
                    "range [{}, {}) exceeds the {extent}-byte extent of {}",
                    request.range.start(),
                    request.range.end(),
                    request.item
                ),
            });
        }
        Ok(bytes.to_vec())
    }

    fn thumbnail_now(&self, item: &ItemId) -> Result<Option<Thumbnail>, SourceError> {
        let playback = lock(&self.playback);
        let revision = playback.revision;
        let tree = self.tree_at(revision)?;

        let Some(found) = tree.get(item) else {
            return Err(SourceError::NotFound {
                detail: format!("no item {item} at revision {revision}"),
            });
        };
        if let ItemContent::File(FileFacts {
            availability: ContentAvailability::Restricted,
            ..
        }) = &found.content
        {
            // POL-4: restricted content is restricted through every door,
            // and a thumbnail is a door.
            return Err(SourceError::Restricted {
                detail: format!("item {item} is restricted at the source"),
            });
        }
        Ok(self.script.thumbnails.get(item).cloned())
    }

    // --- delivery ----------------------------------------------------------

    async fn fetch_inner(
        &self,
        request: &FetchRequest,
        sink: &mut dyn ContentSink,
        guard: &mut CallGuard,
    ) -> Result<(), SourceError> {
        let race = match self.gate(Operation::Fetch, Some(&request.item)).await {
            Effect::Proceed => None,
            Effect::Fail(error) => return Err(error),
            Effect::VersionRace {
                after_bytes,
                current,
            } => Some((after_bytes, current)),
        };

        let bytes = self.fetch_content(request)?;
        let total = request.range.len();
        // A race cut past the range's end can never be reached; clamping
        // makes `after_bytes: u64::MAX` mean "conflict at the very end"
        // rather than "no conflict at all".
        let cut = race.map(|(after, current)| (min(after, total), current));

        if let Some((0, current)) = &cut {
            return Err(version_race_error(current.clone(), 0));
        }

        let mut sizer = ChunkSizer::new(self.script.chunks, self.chunk_seed(request));
        let mut sent = 0u64;
        while sent < total {
            let mut size = sizer.next(total - sent);
            if let Some((cut_at, _)) = &cut
                && sent + size > *cut_at
            {
                size = cut_at - sent;
            }

            let offset = request.range.start() + sent;
            let slice = slice_of(&bytes, offset, size)?;
            let chunk =
                ContentChunk::new(offset, slice).map_err(|invalid| SourceError::Internal {
                    detail: format!("fake produced an invalid chunk: {invalid}"),
                })?;

            let control = sink.accept(chunk);
            // Record before reacting: the bytes are the sink's whether or
            // not it just asked to stop, and a cancellation report that
            // undercounts them would misdescribe the side effect.
            guard.record_delivered(size);
            sent += size;

            if control == SinkControl::Stop {
                return Err(SourceError::Cancelled {
                    detail: format!("sink stopped delivery after {sent} bytes"),
                });
            }
            if let Some((cut_at, current)) = &cut
                && sent >= *cut_at
            {
                return Err(version_race_error(current.clone(), sent));
            }
            if sent < total {
                // The between-chunks cancellation point: a caller that
                // stops polling here drops the future mid-delivery, and
                // the guard records how far the bytes got (SYNC-043).
                Yield::once().await;
            }
        }
        Ok(())
    }

    /// The seed for one fetch's chunk boundaries.
    ///
    /// Folded from the script seed and the request rather than kept as
    /// running generator state: two fetches of the same range must chunk
    /// identically no matter what ran between them, and a source shared
    /// across concurrent callers has no meaningful "next" draw to hand out.
    fn chunk_seed(&self, request: &FetchRequest) -> u64 {
        self.script.seed
            ^ fnv1a(request.item.as_bytes())
            ^ fnv1a(request.version.as_str().as_bytes())
            ^ request.range.start().rotate_left(17)
            ^ request.range.len().rotate_left(33)
    }

    // --- tokens ------------------------------------------------------------

    fn tree_at(&self, revision: u32) -> Result<&Tree, SourceError> {
        self.script
            .tree_at(revision)
            .ok_or_else(|| SourceError::Internal {
                detail: format!("script has no revision {revision}"),
            })
    }

    fn mint_cursor(&self, revision: u32) -> Result<ChangeCursor, SourceError> {
        ChangeCursor::new(
            self.script.scope,
            format!("{CURSOR_PREFIX}{revision}").into_bytes(),
        )
        .map_err(|invalid| SourceError::Internal {
            detail: format!("fake minted an invalid cursor: {invalid}"),
        })
    }

    /// Parses a cursor this source could have minted, rejecting everything
    /// else explicitly (SYNC-004).
    fn parse_cursor(&self, cursor: &ChangeCursor, latest: u32) -> Result<u32, SourceError> {
        cursor
            .require_scope(self.script.scope)
            .map_err(|mismatch| SourceError::CursorRejected {
                detail: mismatch.to_string(),
            })?;

        let payload = cursor.payload();
        if payload.is_empty() {
            // "Nothing observed yet" — a valid position, and the natural
            // way to ask for the feed from its beginning.
            return Ok(0);
        }

        let rejected = |reason: &str| SourceError::CursorRejected {
            detail: format!("cursor payload is not one this source minted: {reason}"),
        };
        let text = std::str::from_utf8(payload).map_err(|_| rejected("not UTF-8"))?;
        let position = text
            .strip_prefix(CURSOR_PREFIX)
            .ok_or_else(|| rejected("unknown format"))?
            .parse::<u32>()
            .map_err(|_| rejected("unparseable position"))?;

        if position > latest {
            return Err(SourceError::CursorRejected {
                detail: format!("cursor at revision {position} is ahead of the source at {latest}"),
            });
        }
        Ok(position)
    }
}

impl DriveSource for FakeSource {
    fn scope(&self) -> AccountScope {
        self.script.scope
    }

    fn root(&self) -> SourceFuture<'_, SourceItem> {
        let guard = self.recorder.begin(Call::Root);
        Box::pin(async move {
            let result = match self.gate(Operation::Root, None).await {
                Effect::Fail(error) => Err(error),
                _ => self.root_now(),
            };
            guard.finish(result)
        })
    }

    fn children(&self, parent: ItemId, request: PageRequest) -> SourceFuture<'_, ItemPage> {
        let guard = self.recorder.begin(Call::Children {
            parent: parent.clone(),
            request: request.clone(),
        });
        Box::pin(async move {
            let result = match self.gate(Operation::Children, Some(&parent)).await {
                Effect::Fail(error) => Err(error),
                _ => self.children_now(&parent, &request),
            };
            guard.finish(result)
        })
    }

    fn latest_cursor(&self) -> SourceFuture<'_, ChangeCursor> {
        let guard = self.recorder.begin(Call::LatestCursor);
        Box::pin(async move {
            let result = match self.gate(Operation::LatestCursor, None).await {
                Effect::Fail(error) => Err(error),
                _ => self.latest_cursor_now(),
            };
            guard.finish(result)
        })
    }

    fn changes(&self, cursor: ChangeCursor) -> SourceFuture<'_, ChangePage> {
        let guard = self.recorder.begin(Call::Changes {
            cursor: cursor.clone(),
        });
        Box::pin(async move {
            let result = match self.gate(Operation::Changes, None).await {
                Effect::Fail(error) => Err(error),
                _ => self.changes_now(&cursor),
            };
            guard.finish(result)
        })
    }

    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        let mut guard = self.recorder.begin(Call::Fetch {
            request: request.clone(),
        });
        Box::pin(async move {
            let result = self.fetch_inner(&request, sink, &mut guard).await;
            guard.finish(result)
        })
    }

    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>> {
        let guard = self.recorder.begin(Call::Thumbnail {
            item: item.clone(),
            spec,
        });
        Box::pin(async move {
            let result = match self.gate(Operation::Thumbnail, Some(&item)).await {
                Effect::Fail(error) => Err(error),
                _ => self.thumbnail_now(&item),
            };
            guard.finish(result)
        })
    }
}

/// Cuts the next chunk's size from a [`ChunkPlan`].
#[derive(Debug)]
struct ChunkSizer {
    plan: ChunkPlan,
    rng: SplitMix64,
}

impl ChunkSizer {
    fn new(plan: ChunkPlan, seed: u64) -> Self {
        Self {
            plan,
            rng: SplitMix64::new(seed),
        }
    }

    /// The next chunk size, never zero and never past `remaining`.
    fn next(&mut self, remaining: u64) -> u64 {
        let size = match self.plan {
            ChunkPlan::Whole => remaining,
            ChunkPlan::Fixed(size) => size.get(),
            ChunkPlan::Seeded { max } => self.rng.next_in_range(max.get()),
        };
        min(size.max(1), remaining)
    }
}

fn version_race_error(current: Option<ContentVersion>, sent: u64) -> SourceError {
    SourceError::VersionConflict {
        current,
        detail: format!("content changed after {sent} bytes were delivered"),
    }
}

/// `bytes[offset .. offset + len]`, without indexing panics.
fn slice_of(bytes: &[u8], offset: u64, len: u64) -> Result<&[u8], SourceError> {
    let out_of_range = || SourceError::Internal {
        detail: format!(
            "fake read [{offset}, {}) past its own content",
            offset + len
        ),
    };
    let start = usize::try_from(offset).map_err(|_| out_of_range())?;
    let end = usize::try_from(offset.saturating_add(len)).map_err(|_| out_of_range())?;
    bytes.get(start..end).ok_or_else(out_of_range)
}

fn mint_page_token(revision: u32, offset: usize) -> Result<PageToken, SourceError> {
    PageToken::new(format!(
        "{PAGE_TOKEN_PREFIX}{revision}{PAGE_TOKEN_OFFSET}{offset}"
    ))
    .map_err(|invalid| SourceError::Internal {
        detail: format!("fake minted an invalid page token: {invalid}"),
    })
}

/// Parses a page token, refusing one minted at another revision.
///
/// The refusal is the point: a token from revision 2 presented at revision
/// 3 names a snapshot this source has stopped serving, and SYNC-003 says
/// reject rather than splice.
fn parse_page_token(token: &PageToken, revision: u32) -> Result<usize, SourceError> {
    let rejected = |reason: &str| SourceError::CursorRejected {
        detail: format!("page token {:?} rejected: {reason}", token.as_str()),
    };

    let rest = token
        .as_str()
        .strip_prefix(PAGE_TOKEN_PREFIX)
        .ok_or_else(|| rejected("not a token this source minted"))?;
    let (minted_at, offset) = rest
        .split_once(PAGE_TOKEN_OFFSET)
        .ok_or_else(|| rejected("malformed"))?;
    let minted_at = minted_at
        .parse::<u32>()
        .map_err(|_| rejected("unparseable revision"))?;
    let offset = offset
        .parse::<usize>()
        .map_err(|_| rejected("unparseable offset"))?;

    if minted_at != revision {
        return Err(rejected(&format!(
            "minted against revision {minted_at}, the source now serves {revision}; \
             re-enumerate from the first page"
        )));
    }
    Ok(offset)
}
