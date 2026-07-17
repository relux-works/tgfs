//! The script: what the fake source knows, and what changes about it.
//!
//! A [`SourceScript`] is a whole backend written down — a base tree, the
//! content behind each file, the change batches that move the tree forward,
//! and the faults that interrupt any of it. [`FakeSource`](crate::FakeSource)
//! is the machine that plays it. Nothing in a script is time-dependent or
//! random: two runs of the same script produce the same bytes, the same
//! page boundaries, the same errors, in the same order.
//!
//! # Revisions replace the clock
//!
//! A real backend changes when Telegram says so. This one changes when the
//! test says so: batch `k` moves the source from revision `k` to `k + 1`,
//! and only [`FakeSource::advance`](crate::FakeSource::advance) applies it.
//! Everything the contract makes time-sensitive becomes a scriptable
//! sequence — a version race is `advance` between a fetch's start and its
//! next chunk; a rejected page token is `advance` mid-enumeration. There is
//! no scheduler to lose a race against.
//!
//! # `build` validates, so the fake never guesses
//!
//! A fake that met an inconsistency at test time — a file with no bytes, a
//! batch adding a child to a parent that does not exist yet — would have to
//! invent a failure, and the test would be asserting against the fake's
//! improvisation rather than the contract. [`ScriptBuilder::build`] instead
//! replays every batch through the same [`Tree`](crate::tree) the fake uses
//! and rejects the script up front. Past `build`, every [`SourceError`] the
//! fake produces is one the *contract* specifies or the script asked for.

// `ScriptError` is 168 bytes: three of its variants name two `ItemId`s, and
// an `ItemId` is 80 (a typed key plus its canonical bytes). `result_large_err`
// is about a fat `Err` taxing the *success* path of a frequently-called
// function, and neither half of that applies here.
//
// It costs nothing: `Result<SourceScript, ScriptError>` measures 288 bytes —
// exactly `size_of::<SourceScript>()` — so the error rides inside the
// footprint the `Ok` payload already needs. And it is not on a hot path:
// every function below runs once per scripted item while a fixture is being
// constructed, before a test has done anything.
//
// The lint's remedies both cost more than they save: boxing puts an
// allocation and a deref in front of a value that is already free, and
// shrinking the variants means storing identities as strings, trading the
// typed `ItemId` a caller can assert against for bytes nothing ever copies.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::num::NonZeroU64;

use gramdrive_model::identity::{AccountScope, ItemId};
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{ContentAvailability, ItemChange, ItemContent, SourceItem, Thumbnail};

use crate::fault::{Effect, Fault, Operation};
use crate::tree::Tree;

/// Default upper bound on a seeded chunk, in bytes.
pub const DEFAULT_MAX_CHUNK_BYTES: u64 = 4096;

/// Default script seed.
pub const DEFAULT_SEED: u64 = 0x6772_616d_6472_6976;

/// How a fetch cuts its delivery into chunks.
///
/// Chunking is where a source is most easily and least visibly wrong: a
/// caller that only ever sees one chunk per fetch has never exercised its
/// own reassembly, and one that sees the same boundaries every time has
/// tested one boundary. [`Seeded`](ChunkPlan::Seeded) is the default for
/// that reason — varied boundaries, identical on every run of the same
/// seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkPlan {
    /// One chunk for the whole range. The simplest delivery, and the one
    /// to pick when a test asserts on chunk count rather than reassembly.
    Whole,
    /// Fixed-size chunks; the last is short when the range does not divide
    /// evenly.
    Fixed(NonZeroU64),
    /// Sizes drawn from the script's seed, each in `1..=max`.
    Seeded {
        /// Upper bound on one chunk.
        max: NonZeroU64,
    },
}

impl Default for ChunkPlan {
    fn default() -> Self {
        Self::Seeded {
            // Non-zero literal; the fallback is unreachable and exists
            // only because `NonZeroU64::new` is not const-callable here
            // without an unwrap the workspace lints forbid.
            max: NonZeroU64::new(DEFAULT_MAX_CHUNK_BYTES).unwrap_or(NonZeroU64::MIN),
        }
    }
}

/// One version of one item's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Blob {
    pub(crate) version: ContentVersion,
    pub(crate) bytes: Vec<u8>,
}

/// A complete, validated description of a fake backend.
///
/// Build one with [`ScriptBuilder`]; play it with
/// [`FakeSource::new`](crate::FakeSource::new). Scripts are immutable and
/// cheap to share — several sources may play the same script, each with its
/// own revision and its own recording.
#[derive(Debug, Clone)]
pub struct SourceScript {
    pub(crate) scope: AccountScope,
    pub(crate) root: ItemId,
    /// The tree at every revision: index `k` is the state after `k`
    /// batches. Materialized at build time rather than folded at test time
    /// so that [`FakeSource::advance`](crate::FakeSource::advance) is a
    /// bounds check instead of an operation that could fail — every apply
    /// already succeeded here, or `build` returned the error.
    pub(crate) revisions: Vec<Tree>,
    pub(crate) batches: Vec<Vec<ItemChange>>,
    pub(crate) blobs: HashMap<ItemId, Vec<Blob>>,
    pub(crate) thumbnails: HashMap<ItemId, Thumbnail>,
    pub(crate) faults: Vec<Fault>,
    pub(crate) seed: u64,
    pub(crate) chunks: ChunkPlan,
}

impl SourceScript {
    /// A builder for a script serving `scope`.
    pub fn builder(scope: AccountScope) -> ScriptBuilder {
        ScriptBuilder::new(scope)
    }

    /// The account and namespace epoch this script serves.
    pub fn scope(&self) -> AccountScope {
        self.scope
    }

    /// The account root's identity.
    pub fn root_id(&self) -> &ItemId {
        &self.root
    }

    /// How many change batches the script carries — the highest revision
    /// [`FakeSource::advance`](crate::FakeSource::advance) can reach.
    pub fn batch_count(&self) -> u32 {
        // A script with 2^32 batches is not a fixture anyone wrote.
        u32::try_from(self.batches.len()).unwrap_or(u32::MAX)
    }

    /// The tree at `revision`, or `None` past the last one.
    pub(crate) fn tree_at(&self, revision: u32) -> Option<&Tree> {
        self.revisions.get(usize::try_from(revision).ok()?)
    }

    /// The batch that moves `revision` to `revision + 1`.
    pub(crate) fn batch_at(&self, revision: u32) -> Option<&[ItemChange]> {
        self.batches
            .get(usize::try_from(revision).ok()?)
            .map(Vec::as_slice)
    }

    /// The bytes of `item` at `version`, if the script carries them.
    pub(crate) fn blob(&self, item: &ItemId, version: &ContentVersion) -> Option<&[u8]> {
        find_blob(&self.blobs, item, version)
    }
}

fn find_blob<'a>(
    blobs: &'a HashMap<ItemId, Vec<Blob>>,
    item: &ItemId,
    version: &ContentVersion,
) -> Option<&'a [u8]> {
    blobs
        .get(item)?
        .iter()
        .find(|blob| &blob.version == version)
        .map(|blob| blob.bytes.as_slice())
}

/// Assembles a [`SourceScript`].
///
/// Order matters in exactly one place: base items must be added after their
/// parents, and batches play in the order they are added. Everything else —
/// content, thumbnails, faults — may be registered whenever.
#[derive(Debug, Clone)]
pub struct ScriptBuilder {
    scope: AccountScope,
    base: Vec<SourceItem>,
    batches: Vec<Vec<ItemChange>>,
    blobs: HashMap<ItemId, Vec<Blob>>,
    thumbnails: HashMap<ItemId, Thumbnail>,
    faults: Vec<Fault>,
    seed: u64,
    chunks: ChunkPlan,
}

impl ScriptBuilder {
    /// An empty script for `scope`.
    pub fn new(scope: AccountScope) -> Self {
        Self {
            scope,
            base: Vec::new(),
            batches: Vec::new(),
            blobs: HashMap::new(),
            thumbnails: HashMap::new(),
            faults: Vec::new(),
            seed: DEFAULT_SEED,
            chunks: ChunkPlan::default(),
        }
    }

    /// Adds one item to the base tree (revision 0).
    pub fn item(mut self, item: SourceItem) -> Self {
        self.base.push(item);
        self
    }

    /// Adds several base-tree items, parents first.
    pub fn items(mut self, items: impl IntoIterator<Item = SourceItem>) -> Self {
        self.base.extend(items);
        self
    }

    /// Registers the bytes served for one version of one item.
    ///
    /// Register every version a file ever holds, including versions
    /// introduced by later batches: a fetch pinned to a superseded version
    /// still has to be answerable, because answering it with
    /// [`SourceError::VersionConflict`] is a decision the source makes
    /// about the *current* version, not about missing data.
    ///
    /// [`SourceError::VersionConflict`]: gramdrive_source::SourceError::VersionConflict
    pub fn content(
        mut self,
        item: &ItemId,
        version: ContentVersion,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        let blobs = self.blobs.entry(item.clone()).or_default();
        let bytes = bytes.into();
        match blobs.iter_mut().find(|blob| blob.version == version) {
            Some(existing) => existing.bytes = bytes,
            None => blobs.push(Blob { version, bytes }),
        }
        self
    }

    /// Registers the thumbnail served for one item.
    ///
    /// An item with none registered answers `Ok(None)` — "this item has no
    /// thumbnail" is a normal answer, not an error.
    pub fn thumbnail(mut self, item: &ItemId, thumbnail: Thumbnail) -> Self {
        self.thumbnails.insert(item.clone(), thumbnail);
        self
    }

    /// Appends one change batch — one step of the feed, and one revision.
    pub fn batch(mut self, changes: impl IntoIterator<Item = ItemChange>) -> Self {
        self.batches.push(changes.into_iter().collect());
        self
    }

    /// Registers a scripted fault. See [`crate::fault`].
    pub fn fault(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    /// Sets the seed behind [`ChunkPlan::Seeded`]. Defaults to
    /// [`DEFAULT_SEED`].
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Sets how fetches chunk their delivery. Defaults to
    /// [`ChunkPlan::Seeded`] with [`DEFAULT_MAX_CHUNK_BYTES`].
    pub fn chunks(mut self, plan: ChunkPlan) -> Self {
        self.chunks = plan;
        self
    }

    /// Validates the script and freezes it.
    ///
    /// Checks, in order: the base tree is a tree with exactly one root;
    /// every fault is targetable; every batch applies cleanly at its own
    /// revision; and every fetchable file, at every revision it exists at,
    /// has bytes matching its declared size.
    pub fn build(self) -> Result<SourceScript, ScriptError> {
        let root = self.validate_root()?;
        self.validate_faults()?;

        // Replay the whole script once, here, and keep every revision it
        // passes through. Content is checked at each one: a file whose
        // bytes arrive only at revision 3 must not be fetchable-but-empty
        // at revision 0.
        let mut tree = Tree::default();
        for item in &self.base {
            tree.insert(item.clone())?;
        }
        validate_content(&root, &self.blobs, &tree)?;

        let mut revisions = Vec::with_capacity(self.batches.len() + 1);
        revisions.push(tree.clone());
        for batch in &self.batches {
            for change in batch {
                tree.apply(change.clone())?;
            }
            validate_content(&root, &self.blobs, &tree)?;
            revisions.push(tree.clone());
        }

        Ok(SourceScript {
            scope: self.scope,
            root,
            revisions,
            batches: self.batches,
            blobs: self.blobs,
            thumbnails: self.thumbnails,
            faults: self.faults,
            seed: self.seed,
            chunks: self.chunks,
        })
    }

    fn validate_root(&self) -> Result<ItemId, ScriptError> {
        let mut roots = self.base.iter().filter(|item| item.parent.is_none());
        let root = roots.next().ok_or(ScriptError::NoRoot)?;
        if let Some(second) = roots.next() {
            return Err(ScriptError::MultipleRoots {
                first: root.id.clone(),
                second: second.id.clone(),
            });
        }
        if !root.is_directory() {
            return Err(ScriptError::RootNotDirectory {
                item: root.id.clone(),
            });
        }
        Ok(root.id.clone())
    }

    fn validate_faults(&self) -> Result<(), ScriptError> {
        for fault in &self.faults {
            let targets_item = matches!(
                fault.operation,
                Operation::Children | Operation::Fetch | Operation::Thumbnail
            );
            if fault.item.is_some() && !targets_item {
                return Err(ScriptError::FaultItemFilterOnAccountOperation {
                    operation: fault.operation,
                });
            }
            if matches!(fault.effect, Effect::VersionRace { .. })
                && fault.operation != Operation::Fetch
            {
                return Err(ScriptError::VersionRaceOutsideFetch {
                    operation: fault.operation,
                });
            }
        }
        Ok(())
    }
}

/// Every fetchable file reachable from `root` must have bytes for its
/// current version, of its declared size.
///
/// Reachability matters: an item detached by a batch is gone as far as the
/// contract is concerned, and holding a script to the content of items no
/// caller can reach would reject valid fixtures.
fn validate_content(
    root: &ItemId,
    blobs: &HashMap<ItemId, Vec<Blob>>,
    tree: &Tree,
) -> Result<(), ScriptError> {
    for item in reachable_items(root, tree) {
        let ItemContent::File(facts) = &item.content else {
            continue;
        };
        if facts.availability != ContentAvailability::Fetchable {
            continue;
        }
        let Some(bytes) = find_blob(blobs, &item.id, &facts.content_version) else {
            return Err(ScriptError::MissingContent {
                item: item.id.clone(),
                version: facts.content_version.clone(),
            });
        };
        let actual = bytes.len() as u64;
        if let Some(declared) = facts.size
            && declared != actual
        {
            return Err(ScriptError::SizeMismatch {
                item: item.id.clone(),
                declared,
                actual,
            });
        }
    }
    Ok(())
}

/// Every item reachable from `root`.
fn reachable_items<'a>(root: &ItemId, tree: &'a Tree) -> Vec<&'a SourceItem> {
    let mut found = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(id) = pending.pop() {
        let Some(item) = tree.get(&id) else {
            continue;
        };
        found.push(item);
        pending.extend(tree.children_of(&id).iter().cloned());
    }
    found
}

/// Why a script is not playable.
///
/// Every variant is a fixture bug, not a contract failure: these are
/// reported by [`ScriptBuilder::build`] before any test runs, so that no
/// [`SourceError`](gramdrive_source::SourceError) the fake produces later
/// is an artifact of a malformed script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
    /// No base item has `parent: None`.
    NoRoot,
    /// More than one base item claims to be the root.
    MultipleRoots {
        /// The first parentless item.
        first: ItemId,
        /// The second — the one that makes the tree a forest.
        second: ItemId,
    },
    /// The root is a file. Only a directory can be enumerated.
    RootNotDirectory {
        /// The offending root.
        item: ItemId,
    },
    /// Two base items share an identity.
    DuplicateItem {
        /// The repeated identity.
        item: ItemId,
    },
    /// An item names a parent that does not exist at its revision.
    UnknownParent {
        /// The item with the dangling parent.
        item: ItemId,
        /// The parent that is not there.
        parent: ItemId,
    },
    /// An item names a file as its parent.
    ParentNotDirectory {
        /// The item.
        item: ItemId,
        /// The file it claims as a parent.
        parent: ItemId,
    },
    /// An upsert moved an item to or from parentlessness. Rootness is
    /// structural: an item cannot become the root, and the root cannot
    /// become a child.
    RootReparented,
    /// A batch removed an item that does not exist at that revision.
    RemovedUnknownItem {
        /// The identity the batch tried to remove.
        item: ItemId,
    },
    /// A fetchable file has no bytes registered for its content version.
    MissingContent {
        /// The file.
        item: ItemId,
        /// The version whose bytes are missing.
        version: ContentVersion,
    },
    /// A file's declared size disagrees with its registered bytes. The
    /// contract lets a source omit a size, never misstate one.
    SizeMismatch {
        /// The file.
        item: ItemId,
        /// The size the item declares.
        declared: u64,
        /// The size of the registered bytes.
        actual: u64,
    },
    /// A fault filters by item on an operation that targets no item; it
    /// could never fire.
    FaultItemFilterOnAccountOperation {
        /// The operation the filter was attached to.
        operation: Operation,
    },
    /// A [`Effect::VersionRace`] fault targets something other than
    /// `fetch`; only a fetch delivers bytes to race against.
    VersionRaceOutsideFetch {
        /// The operation the race was attached to.
        operation: Operation,
    },
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRoot => f.write_str("script has no root item (none has `parent: None`)"),
            Self::MultipleRoots { first, second } => write!(
                f,
                "script has two root items, {first} and {second}; exactly one item may be parentless"
            ),
            Self::RootNotDirectory { item } => {
                write!(
                    f,
                    "root item {item} is a file; the root must be a directory"
                )
            }
            Self::DuplicateItem { item } => write!(f, "item {item} is declared twice"),
            Self::UnknownParent { item, parent } => {
                write!(f, "item {item} names parent {parent}, which does not exist")
            }
            Self::ParentNotDirectory { item, parent } => {
                write!(
                    f,
                    "item {item} names {parent} as its parent, but that is a file"
                )
            }
            Self::RootReparented => f.write_str(
                "an upsert changed an item to or from parentless; rootness is structural",
            ),
            Self::RemovedUnknownItem { item } => {
                write!(
                    f,
                    "a batch removes item {item}, which does not exist at that revision"
                )
            }
            Self::MissingContent { item, version } => write!(
                f,
                "fetchable file {item} has no content registered for version {version}"
            ),
            Self::SizeMismatch {
                item,
                declared,
                actual,
            } => write!(
                f,
                "file {item} declares size {declared} but its registered content is {actual} bytes"
            ),
            Self::FaultItemFilterOnAccountOperation { operation } => write!(
                f,
                "a fault on {operation:?} filters by item, but {operation:?} targets no item; it could never fire"
            ),
            Self::VersionRaceOutsideFetch { operation } => write!(
                f,
                "a version-race fault targets {operation:?}; only fetch delivers bytes to race against"
            ),
        }
    }
}

impl std::error::Error for ScriptError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::Occurrence;
    use crate::fixture;
    use gramdrive_source::{DirectoryKind, FileKind, SourceError};

    fn root() -> SourceItem {
        fixture::directory(
            fixture::account_root_id(fixture::scope()),
            None,
            "Account",
            "m1",
            DirectoryKind::Root,
        )
        .unwrap()
    }

    fn chat() -> SourceItem {
        fixture::directory(
            fixture::chat_id(fixture::scope(), 100),
            Some(fixture::account_root_id(fixture::scope())),
            "Team",
            "m2",
            DirectoryKind::Chat,
        )
        .unwrap()
    }

    fn photo(version: &str, content_version: &str, size: u64) -> SourceItem {
        fixture::file(
            fixture::attachment_id(fixture::scope(), 100, 5, 0),
            fixture::chat_id(fixture::scope(), 100),
            "photo.jpg",
            version,
            content_version,
            size,
            FileKind::Attachment,
        )
        .unwrap()
    }

    fn photo_id() -> ItemId {
        fixture::attachment_id(fixture::scope(), 100, 5, 0)
    }

    fn version(token: &str) -> ContentVersion {
        ContentVersion::new(token).unwrap()
    }

    #[test]
    fn a_minimal_script_builds() {
        let script = SourceScript::builder(fixture::scope())
            .item(root())
            .build()
            .expect("a lone root is a valid script");
        assert_eq!(script.scope(), fixture::scope());
        assert_eq!(
            script.root_id(),
            &fixture::account_root_id(fixture::scope())
        );
        assert_eq!(script.batch_count(), 0);
        assert_eq!(script.seed, DEFAULT_SEED);
    }

    #[test]
    fn defaults_are_seeded_chunking() {
        let script = SourceScript::builder(fixture::scope())
            .item(root())
            .build()
            .unwrap();
        assert_eq!(
            script.chunks,
            ChunkPlan::Seeded {
                max: NonZeroU64::new(DEFAULT_MAX_CHUNK_BYTES).unwrap()
            }
        );
    }

    #[test]
    fn a_script_without_a_root_is_rejected() {
        let error = SourceScript::builder(fixture::scope())
            .item(chat())
            .build()
            .expect_err("no parentless item");
        assert_eq!(error, ScriptError::NoRoot);
        assert!(error.to_string().contains("no root item"));
    }

    #[test]
    fn two_roots_are_rejected() {
        let mut second = chat();
        second.parent = None;
        let error = SourceScript::builder(fixture::scope())
            .items([root(), second])
            .build()
            .expect_err("a forest is not a tree");
        assert!(matches!(error, ScriptError::MultipleRoots { .. }));
    }

    #[test]
    fn a_file_root_is_rejected() {
        let mut file_root = photo("m1", "c1", 4);
        file_root.parent = None;
        let error = SourceScript::builder(fixture::scope())
            .item(file_root)
            .build()
            .expect_err("the root must be enumerable");
        assert!(matches!(error, ScriptError::RootNotDirectory { .. }));
    }

    #[test]
    fn base_items_must_follow_their_parents() {
        let error = SourceScript::builder(fixture::scope())
            .items([root(), photo("m3", "c1", 4)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .build()
            .expect_err("the chat parent was never declared");
        assert!(matches!(error, ScriptError::UnknownParent { .. }));
    }

    #[test]
    fn a_fetchable_file_without_content_is_rejected() {
        let error = SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 4)])
            .build()
            .expect_err("no bytes registered");
        assert_eq!(
            error,
            ScriptError::MissingContent {
                item: photo_id(),
                version: version("c1")
            }
        );
        assert!(error.to_string().contains("no content registered"));
    }

    #[test]
    fn a_declared_size_must_match_the_registered_bytes() {
        let error = SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 99)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .build()
            .expect_err("99 declared, 4 registered");
        assert_eq!(
            error,
            ScriptError::SizeMismatch {
                item: photo_id(),
                declared: 99,
                actual: 4
            }
        );
    }

    #[test]
    fn a_restricted_file_needs_no_content() {
        let restricted = fixture::restricted_file(
            photo_id(),
            fixture::chat_id(fixture::scope(), 100),
            "secret.jpg",
            "m3",
            "c1",
            4,
            FileKind::Attachment,
        )
        .unwrap();
        SourceScript::builder(fixture::scope())
            .items([root(), chat(), restricted])
            .build()
            .expect("POL-4: restricted bytes are never served, so none are needed");
    }

    #[test]
    fn content_is_validated_at_every_revision_a_file_exists_at() {
        // The batch introduces c2 but registers no bytes for it.
        let error = SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 4)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .batch([ItemChange::Upserted(photo("m4", "c2", 4))])
            .build()
            .expect_err("c2 has no bytes");
        assert_eq!(
            error,
            ScriptError::MissingContent {
                item: photo_id(),
                version: version("c2")
            }
        );
    }

    #[test]
    fn a_fully_specified_multi_revision_script_builds() {
        let script = SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 4)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .content(&photo_id(), version("c2"), *b"wxyz!")
            .batch([ItemChange::Upserted(photo("m4", "c2", 5))])
            .batch([ItemChange::Removed(photo_id())])
            .build()
            .expect("every revision is consistent");
        assert_eq!(script.batch_count(), 2);
        assert_eq!(
            script.blob(&photo_id(), &version("c1")),
            Some(b"abcd".as_slice())
        );
        assert_eq!(
            script.blob(&photo_id(), &version("c2")),
            Some(b"wxyz!".as_slice())
        );
        assert_eq!(script.blob(&photo_id(), &version("c9")), None);
    }

    #[test]
    fn a_batch_that_removes_a_ghost_is_rejected() {
        let error = SourceScript::builder(fixture::scope())
            .items([root(), chat()])
            .batch([ItemChange::Removed(photo_id())])
            .build()
            .expect_err("the photo was never there");
        assert!(matches!(error, ScriptError::RemovedUnknownItem { .. }));
    }

    #[test]
    fn re_registering_a_version_replaces_its_bytes() {
        let script = SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 2)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .content(&photo_id(), version("c1"), *b"ok")
            .build()
            .expect("the later registration wins, and matches the declared size");
        assert_eq!(
            script.blob(&photo_id(), &version("c1")),
            Some(b"ok".as_slice())
        );
    }

    #[test]
    fn an_item_filter_on_an_account_operation_is_rejected() {
        let error = SourceScript::builder(fixture::scope())
            .item(root())
            .fault(Fault::on(Operation::LatestCursor).for_item(photo_id()))
            .build()
            .expect_err("latest_cursor targets no item");
        assert_eq!(
            error,
            ScriptError::FaultItemFilterOnAccountOperation {
                operation: Operation::LatestCursor
            }
        );
        assert!(error.to_string().contains("could never fire"));
    }

    #[test]
    fn a_version_race_outside_fetch_is_rejected() {
        let error = SourceScript::builder(fixture::scope())
            .item(root())
            .fault(Fault::on(Operation::Children).version_race(4, None))
            .build()
            .expect_err("only fetch delivers bytes");
        assert_eq!(
            error,
            ScriptError::VersionRaceOutsideFetch {
                operation: Operation::Children
            }
        );
    }

    #[test]
    fn faults_on_targetable_operations_are_accepted() {
        SourceScript::builder(fixture::scope())
            .items([root(), chat(), photo("m3", "c1", 4)])
            .content(&photo_id(), version("c1"), *b"abcd")
            .fault(
                Fault::on(Operation::Fetch)
                    .for_item(photo_id())
                    .occurrence(Occurrence::Nth(1))
                    .fail(SourceError::Unavailable {
                        detail: "offline".to_owned(),
                    }),
            )
            .fault(Fault::on(Operation::Children).for_item(fixture::chat_id(fixture::scope(), 100)))
            .fault(Fault::on(Operation::Thumbnail).for_item(photo_id()))
            .fault(Fault::on(Operation::Changes).delay(2))
            .build()
            .expect("every fault is targetable");
    }
}
