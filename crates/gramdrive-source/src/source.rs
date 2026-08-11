//! The `DriveSource` trait — the operations every backend must satisfy
//! (DEC-003, SYNC-001..005; TASK-260715-1j4ij3).
//!
//! # Shape
//!
//! Methods return [`SourceFuture`] — a boxed, `Send` future — rather than
//! using `async fn`: the engine selects between interchangeable
//! implementations at runtime (local TDLib on desktop, remote for iOS cold
//! hydration — `.spec/architecture.md`), so the trait must be
//! dyn-compatible, which `async fn` in traits is not. Boxing costs one
//! allocation per call against operations that cross a network; nothing
//! here is hot enough to notice.
//!
//! # Cancellation (SYNC-005, SYNC-043, NFR-025)
//!
//! Dropping a returned future *is* the cancellation signal: implementations
//! must reach cancellation points promptly (every await of network or disk
//! work qualifies) and must leave durable state resumable or safely
//! disposable when dropped. Fetch adds the in-band path —
//! [`SinkControl::Stop`](crate::SinkControl::Stop) — for hosts whose
//! cancellation arrives as a callback rather than a dropped task. No
//! operation may block indefinitely; bounded deadlines are the caller's to
//! enforce and the implementation's to respect.
//!
//! # Concurrency
//!
//! Sources are shared (`Send + Sync`, held behind `Arc`); callers may issue
//! concurrent operations. Request coalescing and concurrency *limits* are
//! engine policy (SYNC-046, SEC-031); a source's obligation is only that
//! concurrent calls never corrupt each other's answers.

use std::future::Future;
use std::pin::Pin;

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountScope, ItemId};

use crate::error::SourceError;
use crate::fetch::{ContentSink, FetchRequest, Thumbnail, ThumbnailSpec};
use crate::item::SourceItem;
use crate::page::{ChangePage, ItemPage, PageRequest};

/// A boxed, sendable future resolving to a contract result.
pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SourceError>> + Send + 'a>>;

/// The content-only subset consumed by the transfer engine.
///
/// A local source may compose metadata discovery and content fetching from
/// different adapters while still owning one provider session. Keeping this
/// port narrower than [`DriveSource`] lets the TDLib downloader participate in
/// hydration without inventing enumeration methods it does not own.
pub trait ContentSource: Send + Sync {
    /// Delivers one pinned byte range into `sink`.
    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()>;
}

/// The provider-neutral drive backend contract (DEC-003).
///
/// Implementations live in separate crates — `gramdrive-source-tdjson`,
/// `gramdrive-source-remote` (future), and the deterministic fake in
/// `gramdrive-testkit` — never here and never behind feature flags
/// (DEC-005). Every implementation must pass the one conformance suite
/// (SYNC-002, NFR-002; TASK-260715-3e8q4m).
pub trait DriveSource: Send + Sync {
    /// The account and namespace epoch this source serves. Cursors and
    /// identities minted by this source carry this scope; a
    /// [`ChangeCursor`] presented against a different scope must be
    /// rejected with [`SourceError::CursorRejected`] (SYNC-004).
    fn scope(&self) -> AccountScope;

    /// The account root item — the only item with no parent.
    fn root(&self) -> SourceFuture<'_, SourceItem>;

    /// One page of `parent`'s children (SYNC-003).
    ///
    /// Enumeration is a snapshot: every page of one enumeration reports
    /// the same [`ItemPage::snapshot`], with no duplicate and no missing
    /// child across its pages. A continuation the source can no longer
    /// serve fails with [`SourceError::CursorRejected`]; enumerating a
    /// file item fails with [`SourceError::InvalidRequest`]. Enumeration
    /// never hydrates content (SYNC-040).
    fn children(&self, parent: ItemId, request: PageRequest) -> SourceFuture<'_, ItemPage>;

    /// The cursor denoting the source's current position — the anchor a
    /// fresh baseline pairs with a full enumeration, and the starting
    /// point of change tracking (SYNC-004).
    fn latest_cursor(&self) -> SourceFuture<'_, ChangeCursor>;

    /// Changes observed since `cursor`, in source order (SYNC-022).
    ///
    /// The cursor must carry this source's scope; anything else — foreign
    /// scope, retired namespace epoch, a position the source can no longer
    /// serve — fails with [`SourceError::CursorRejected`], and recovery is
    /// a fresh baseline (SYNC-004, SYNC-023).
    fn changes(&self, cursor: ChangeCursor) -> SourceFuture<'_, ChangePage>;

    /// Delivers exactly `request.range` of `request.item` into `sink`,
    /// pinned to `request.version` — the delivery contract, cancellation
    /// semantics, and failure classes are specified in [`crate::fetch`].
    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()>;

    /// A thumbnail for `item` fitting within `spec`, when the source has
    /// one. `Ok(None)` means "this item has no thumbnail" — a normal
    /// answer, not an error; restricted content fails with
    /// [`SourceError::Restricted`] like any other content access (POL-4).
    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>>;
}

impl<T> ContentSource for T
where
    T: DriveSource + ?Sized,
{
    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        DriveSource::fetch(self, request, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{ContentChunk, FetchProgress, SinkControl};
    use crate::item::{ContentAvailability, DirectoryKind, FileFacts, FileKind, ItemContent};
    use crate::page::{ItemChange, PageToken};
    use gramdrive_model::ByteRange;
    use gramdrive_model::identity::{
        AccountId, AccountKey, CanonicalKey, ItemKey, NamespaceVersion,
    };
    use gramdrive_model::version::{ContentVersion, MetadataVersion};
    use std::num::NonZeroU32;
    use std::task::{Context, Poll, Waker};

    /// Polls a future to completion on the current thread.
    ///
    /// The stub source below never yields `Pending`, so a noop waker and a
    /// bounded poll loop are a complete executor for these tests — no
    /// runtime dependency needed to prove the contract's shape.
    fn block_on<T>(mut future: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        for _ in 0..1024 {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
        unreachable!("stub futures resolve without waiting");
    }

    fn scope() -> AccountScope {
        AccountScope {
            account: AccountKey {
                account_id: AccountId(7),
            },
            namespace_version: NamespaceVersion(1),
        }
    }

    fn root_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Account(AccountKey {
            account_id: AccountId(7),
        }))
        .id()
    }

    fn root_item() -> SourceItem {
        SourceItem {
            id: root_id(),
            parent: None,
            display_name: "Account".to_owned(),
            metadata_version: MetadataVersion::new("m1").unwrap(),
            created_at_ms: None,
            modified_at_ms: None,
            content: ItemContent::Directory(DirectoryKind::Root),
        }
    }

    fn file_item() -> SourceItem {
        SourceItem {
            id: root_id(),
            parent: Some(root_id()),
            display_name: "photo.jpg".to_owned(),
            metadata_version: MetadataVersion::new("m2").unwrap(),
            created_at_ms: None,
            modified_at_ms: None,
            content: ItemContent::File(FileFacts {
                kind: FileKind::Attachment,
                content_version: ContentVersion::new("c1").unwrap(),
                size: Some(8),
                mime_type: Some("image/jpeg".to_owned()),
                availability: ContentAvailability::Fetchable,
            }),
        }
    }

    /// Minimal in-test implementation proving the trait is implementable
    /// and dyn-compatible. The *deterministic* fake with scripted failures
    /// is TASK-260715-3uft8j; this stub only exercises the shape.
    #[derive(Debug)]
    struct StubSource;

    impl DriveSource for StubSource {
        fn scope(&self) -> AccountScope {
            scope()
        }

        fn root(&self) -> SourceFuture<'_, SourceItem> {
            Box::pin(async { Ok(root_item()) })
        }

        fn children(&self, _parent: ItemId, request: PageRequest) -> SourceFuture<'_, ItemPage> {
            Box::pin(async move {
                if request.continuation.is_some() {
                    return Err(SourceError::CursorRejected {
                        detail: "stub serves a single page".to_owned(),
                    });
                }
                Ok(ItemPage {
                    snapshot: MetadataVersion::new("m1").unwrap(),
                    items: vec![file_item()],
                    next: Some(PageToken::new("after:0").unwrap()),
                })
            })
        }

        fn latest_cursor(&self) -> SourceFuture<'_, ChangeCursor> {
            Box::pin(async {
                Ok(ChangeCursor::new(scope(), b"pos:0".to_vec()).expect("payload below cap"))
            })
        }

        fn changes(&self, cursor: ChangeCursor) -> SourceFuture<'_, ChangePage> {
            Box::pin(async move {
                cursor
                    .require_scope(scope())
                    .map_err(|mismatch| SourceError::CursorRejected {
                        detail: mismatch.to_string(),
                    })?;
                Ok(ChangePage {
                    changes: vec![ItemChange::Upserted(file_item())],
                    next: ChangeCursor::new(scope(), b"pos:1".to_vec()).expect("payload below cap"),
                    more_available: false,
                })
            })
        }

        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                let bytes = vec![0xabu8; request.range.len() as usize];
                let chunk = ContentChunk::new(request.range.start(), &bytes)
                    .expect("range is non-empty by construction");
                match sink.accept(chunk) {
                    SinkControl::Continue => Ok(()),
                    SinkControl::Stop => Err(SourceError::Cancelled {
                        detail: "sink stopped delivery".to_owned(),
                    }),
                }
            })
        }

        fn thumbnail(
            &self,
            _item: ItemId,
            _spec: ThumbnailSpec,
        ) -> SourceFuture<'_, Option<Thumbnail>> {
            Box::pin(async { Ok(None) })
        }
    }

    /// A sink that records delivery through the verified accounting and
    /// stops after a configured number of chunks.
    struct CountingSink {
        progress: FetchProgress,
        accept_chunks: usize,
        seen: usize,
    }

    impl ContentSink for CountingSink {
        fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl {
            self.progress
                .record(&chunk)
                .expect("stub delivers contiguously");
            self.seen += 1;
            if self.seen >= self.accept_chunks {
                SinkControl::Stop
            } else {
                SinkControl::Continue
            }
        }
    }

    #[test]
    fn trait_is_dyn_compatible_and_answers_through_the_object() {
        let source: Box<dyn DriveSource> = Box::new(StubSource);
        assert_eq!(source.scope(), scope());

        let root = block_on(source.root()).expect("root resolves");
        assert!(root.is_directory());
        assert_eq!(root.parent, None);

        let page = block_on(source.children(
            root.id.clone(),
            PageRequest::first(NonZeroU32::new(10).unwrap()),
        ))
        .expect("first page resolves");
        assert_eq!(page.items.len(), 1);
        assert!(page.next.is_some());
    }

    #[test]
    fn change_flow_gates_cursors_by_scope() {
        let source: Box<dyn DriveSource> = Box::new(StubSource);
        let cursor = block_on(source.latest_cursor()).expect("cursor resolves");
        assert_eq!(cursor.scope(), scope());

        let page = block_on(source.changes(cursor)).expect("matching scope is served");
        assert_eq!(page.changes.len(), 1);
        assert!(!page.more_available);

        let foreign_scope = AccountScope {
            account: AccountKey {
                account_id: AccountId(99),
            },
            namespace_version: NamespaceVersion(1),
        };
        let foreign = ChangeCursor::new(foreign_scope, Vec::new()).unwrap();
        let err = block_on(source.changes(foreign)).expect_err("foreign scope must be rejected");
        assert!(matches!(err, SourceError::CursorRejected { .. }));
    }

    #[test]
    fn fetch_delivers_into_the_sink_and_honors_stop() {
        let source: Box<dyn DriveSource> = Box::new(StubSource);
        let range = ByteRange::new(16, 24).unwrap();
        let request = FetchRequest {
            item: root_id(),
            version: ContentVersion::new("c1").unwrap(),
            range,
        };

        let mut sink = CountingSink {
            progress: FetchProgress::new(range),
            accept_chunks: usize::MAX,
            seen: 0,
        };
        block_on(source.fetch(request.clone(), &mut sink)).expect("delivery completes");
        assert!(sink.progress.is_complete());

        let mut stopping = CountingSink {
            progress: FetchProgress::new(range),
            accept_chunks: 1,
            seen: 0,
        };
        let err = block_on(source.fetch(request, &mut stopping))
            .expect_err("a stopping sink cancels the fetch");
        assert!(matches!(err, SourceError::Cancelled { .. }));
    }

    #[test]
    fn missing_thumbnail_is_a_normal_answer() {
        let source: Box<dyn DriveSource> = Box::new(StubSource);
        let spec = ThumbnailSpec {
            max_width_px: NonZeroU32::new(256).unwrap(),
            max_height_px: NonZeroU32::new(256).unwrap(),
        };
        let answer = block_on(source.thumbnail(root_id(), spec)).expect("thumbnail resolves");
        assert_eq!(answer, None);
    }
}
