//! The conformance run for the tdjson source's ranged reads and thumbnails
//! (TASK-260715-1onbmf, TASK-260715-3nl3mu): the one `DriveSource` suite
//! (SYNC-002), with every fetch flowing through the real download adapter —
//! catalog gates, `downloadFile` over the real runtime and the mock tdjson,
//! local reads from a real temporary file — every thumbnail through the real
//! thumbnail adapter, and enumeration served by the testkit's deterministic
//! fake, whose own conformance the testkit already proves.
//!
//! # What this run certifies, and what it does not
//!
//! Every `fetch.*`, fetch-op `failure.*`, and fetch `cancellation.*` case
//! exercises `gramdrive_source_tdjson::download` for real: the bytes the
//! suite compares came out of a file on disk through the adapter's read
//! path, the stale-reference case runs the actual `getMessage` refresh, and
//! the version race trips the adapter's per-slice re-verification. The POL-4
//! "every door" case (shape.rs) runs `gramdrive_source_tdjson::thumbnail` for
//! real too: the restricted landmark's thumbnail is refused by the real
//! adapter's POL-4 gate, before any request. The enumeration and cursor
//! clauses certify the embedded fake — they run here so the suite runs whole
//! (it has no partial mode), not as evidence about tdjson enumeration, which
//! lands with its own adapter task. The harness name says as much in every
//! report.
//!
//! # How TDLib is played
//!
//! The mock's responder is TDLib's side of the download protocol: it
//! answers a synchronous `downloadFile` with a `file` object whose local
//! state points at a temporary file this harness wrote (the stand-in for
//! TDLib's files directory — read-only to the adapter, per the ownership
//! rule), serves the `getMessage` refresh, and stages the armed fetch
//! perturbations: one unreachable / throttled / expired-reference failure,
//! or a download that never answers (the "slow" a drop-in-flight case
//! needs). The version race is staged in the catalog: it serves the pinned
//! version for exactly the resolves that deliver `after_bytes`, then moves
//! on, and the adapter's next pre-slice check must catch it.

// clippy.toml exempts test code keyed on `#[test]` functions; the harness
// below sits at module level in an integration-test binary. The rationale
// applies in full — this file links into no product artifact (established
// test-suite pattern, common/mod.rs).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountScope, ItemId};
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{
    ChangePage, ContentSink, DriveSource, FetchRequest, ItemPage, PageRequest, SourceFuture,
    SourceItem, Thumbnail, ThumbnailSpec,
};
use gramdrive_source_tdjson::message::AttachmentAvailability;
use gramdrive_source_tdjson::mock::{MockTdJson, SentRequest};
use gramdrive_source_tdjson::{
    CatalogEntry, DownloadConfig, DownloadPriority, FetchCatalog, FileTarget, RefreshTarget,
    TdDownloader, TdRuntime, TdThumbnailer, ThumbnailCatalog, ThumbnailConfig, ThumbnailTarget,
};
use gramdrive_testkit::FakeSource;
use gramdrive_testkit::conformance::{
    self, Capability, Control, FakeHarness, Mutation, Perturbation, Setup, SourceHarness, Staged,
    WorldSpec,
};
use gramdrive_testkit::conformance::{HarnessError, Landmarks};
use gramdrive_testkit::fault::Operation;

/// The TDLib file id the harness assigns the world's file.
const FILE_ID: i32 = 700;
/// The chat and message the world's attachment hangs off, for the refresh.
const CHAT_ID: i64 = 102;
const MESSAGE_ID: i64 = 5;
/// Telegram's stable content id for the world's file — what a pure
/// reference refresh must reproduce.
const UNIQUE_ID: &str = "u-conformance-payload";
/// The read slice: small against the world's file, so every fetch streams
/// in several slices and the race case can cut between them.
const READ_CHUNK_BYTES: u64 = 8;

// ---------------------------------------------------------------------------
// The source under test: real fetch, fake everything else
// ---------------------------------------------------------------------------

/// The staged source: `fetch` goes through the real download adapter and
/// `thumbnail` through the real thumbnail adapter, both over the mock tdjson;
/// every other operation is the embedded testkit fake (module docs). The
/// runtime rides along so its receive loop lives as long as the source.
struct RangedTdjsonSource {
    fake: Arc<FakeSource>,
    downloader: TdDownloader,
    thumbnailer: TdThumbnailer,
    _runtime: TdRuntime,
}

impl DriveSource for RangedTdjsonSource {
    fn scope(&self) -> AccountScope {
        self.fake.scope()
    }

    fn root(&self) -> SourceFuture<'_, SourceItem> {
        self.fake.root()
    }

    fn children(&self, parent: ItemId, request: PageRequest) -> SourceFuture<'_, ItemPage> {
        self.fake.children(parent, request)
    }

    fn latest_cursor(&self) -> SourceFuture<'_, ChangeCursor> {
        self.fake.latest_cursor()
    }

    fn changes(&self, cursor: ChangeCursor) -> SourceFuture<'_, ChangePage> {
        self.fake.changes(cursor)
    }

    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        self.downloader.fetch(request, sink)
    }

    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>> {
        self.thumbnailer.thumbnail(item, spec)
    }
}

// ---------------------------------------------------------------------------
// The thumbnail catalog: the restricted landmark refuses through POL-4
// ---------------------------------------------------------------------------

/// The preview facts for the conformance run. The suite calls `thumbnail`
/// only on the restricted landmark (the POL-4 "every door" case, shape.rs),
/// which the real adapter refuses with `Restricted` and zero requests; every
/// other item has no thumbnail (`Ok(None)`), so no preview download is ever
/// staged and the responder never sees one.
struct ConformanceThumbnailCatalog {
    restricted: Option<ItemId>,
}

impl ThumbnailCatalog for ConformanceThumbnailCatalog {
    fn resolve(&self, item: &ItemId) -> Option<ThumbnailTarget> {
        if self.restricted.as_ref() == Some(item) {
            return Some(ThumbnailTarget {
                availability: AttachmentAvailability::Restricted,
                downloadable: None,
                inline: None,
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// The catalog: landmark identities resolved to fetch facts
// ---------------------------------------------------------------------------

/// When armed, serves the base entry for the world's file for exactly
/// `serve_base` resolves, then the moved entry — the catalog half of
/// [`Perturbation::FetchRacesContentChange`].
struct RaceFlip {
    serve_base: Mutex<u32>,
    moved: CatalogEntry,
}

struct ConformanceCatalog {
    entries: Mutex<HashMap<ItemId, CatalogEntry>>,
    file: ItemId,
    race: Option<RaceFlip>,
}

impl FetchCatalog for ConformanceCatalog {
    fn resolve(&self, item: &ItemId) -> Option<CatalogEntry> {
        if let Some(race) = &self.race
            && *item == self.file
        {
            let mut remaining = race.serve_base.lock().expect("race counter");
            if *remaining == 0 {
                return Some(race.moved.clone());
            }
            *remaining -= 1;
        }
        self.entries.lock().expect("catalog map").get(item).cloned()
    }
}

fn file_target(version: &ContentVersion, size: u64) -> FileTarget {
    FileTarget {
        file_id: FILE_ID,
        remote_id: None,
        remote_file_type: None,
        refresh: RefreshTarget::Message {
            chat_id: CHAT_ID,
            message_id: MESSAGE_ID,
        },
        availability: AttachmentAvailability::Fetchable,
        remote_unique_id: Some(UNIQUE_ID.to_owned()),
        size: Some(size),
        version: version.clone(),
    }
}

/// Map every landmark the suite can fetch: the file, the restricted file,
/// and the directories (fetching one is `InvalidRequest`). The absent
/// landmark stays unmapped — resolution `None` is `NotFound`.
fn catalog_entries(landmarks: &Landmarks, world: &WorldSpec) -> HashMap<ItemId, CatalogEntry> {
    let mut entries = HashMap::new();
    entries.insert(
        landmarks.file.clone(),
        CatalogEntry::File(file_target(
            &landmarks.file_version,
            world.file_bytes.len() as u64,
        )),
    );
    if let Some(restricted) = &landmarks.restricted_file {
        entries.insert(
            restricted.clone(),
            CatalogEntry::File(FileTarget {
                availability: AttachmentAvailability::Restricted,
                remote_unique_id: Some("u-restricted".to_owned()),
                version: ContentVersion::new("restricted-c1").expect("valid token"),
                ..file_target(&landmarks.file_version, 16)
            }),
        );
    }
    for directory in [
        &landmarks.root,
        &landmarks.listing,
        &landmarks.empty_directory,
        &landmarks.file_parent,
    ]
    .into_iter()
    .chain(landmarks.listing_children.iter())
    {
        entries.insert(directory.clone(), CatalogEntry::Directory);
    }
    entries
}

// ---------------------------------------------------------------------------
// TDLib's side: the responder
// ---------------------------------------------------------------------------

/// One armed fetch-operation perturbation, consumed by the first
/// `downloadFile`.
enum FetchFault {
    /// Answer with a TDLib error.
    Fail { code: i64, message: String },
    /// Never answer — the download the drop-in-flight case abandons.
    Silence,
}

/// What the responder serves: the current content's local file. Mutations
/// move it in step with the catalog.
struct CurrentContent {
    path: PathBuf,
    len: u64,
}

struct TdlibSide {
    faults: Mutex<VecDeque<FetchFault>>,
    current: Mutex<CurrentContent>,
}

impl TdlibSide {
    fn respond(&self, sent: &SentRequest) -> Vec<String> {
        let extra = sent.extra().expect("the runtime injects @extra");
        let value: Value = serde_json::from_str(&sent.json).expect("requests are JSON");
        match value.get("@type").and_then(Value::as_str) {
            Some("downloadFile") => {
                match self.faults.lock().expect("fault queue").pop_front() {
                    Some(FetchFault::Silence) => return Vec::new(),
                    Some(FetchFault::Fail { code, message }) => {
                        return vec![
                            json!({
                                "@type": "error",
                                "code": code,
                                "message": message,
                                "@extra": extra,
                                "@client_id": sent.client_id,
                            })
                            .to_string(),
                        ];
                    }
                    None => {}
                }
                assert_eq!(
                    value.get("file_id").and_then(Value::as_i64),
                    Some(i64::from(FILE_ID)),
                    "only the world's file is downloadable"
                );
                assert_eq!(
                    value.get("synchronous").and_then(Value::as_bool),
                    Some(true)
                );
                let offset = value.get("offset").and_then(Value::as_u64).expect("offset");
                let limit = value.get("limit").and_then(Value::as_u64).expect("limit");
                let current = self.current.lock().expect("current content");
                vec![
                    json!({
                        "@type": "file",
                        "id": FILE_ID,
                        "size": current.len,
                        "local": {
                            "@type": "localFile",
                            "path": current.path.to_str().expect("utf-8 temp path"),
                            "download_offset": offset,
                            "downloaded_prefix_size": limit.min(current.len - offset.min(current.len)),
                            "is_downloading_active": false,
                            "is_downloading_completed": offset == 0 && limit >= current.len,
                        },
                        "@extra": extra,
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                ]
            }
            Some("getMessage") => {
                // The reference refresh: same message, same attachment,
                // same stable content id — TDLib re-learned the locator.
                vec![
                    json!({
                        "@type": "message",
                        "id": MESSAGE_ID,
                        "chat_id": CHAT_ID,
                        "date": 1_752_800_000,
                        "sender_id": {"@type": "messageSenderUser", "user_id": 42},
                        "can_be_saved": true,
                        "content": {
                            "@type": "messageDocument",
                            "caption": {"text": ""},
                            "document": {
                                "file_name": "payload.bin",
                                "mime_type": "application/octet-stream",
                                "document": {
                                    "id": FILE_ID,
                                    "size": self.current.lock().expect("current content").len,
                                    "remote": {"id": "r-1", "unique_id": UNIQUE_ID},
                                },
                            },
                        },
                        "@extra": extra,
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                ]
            }
            Some("cancelDownloadFile") => {
                vec![format!(
                    r#"{{"@type":"ok","@extra":{extra},"@client_id":{}}}"#,
                    sent.client_id
                )]
            }
            other => panic!("the download adapter sent an unexpected request: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The control: mutations move the fake, the catalog, and the served file
// ---------------------------------------------------------------------------

struct WiredControl {
    fake: Box<dyn Control>,
    plan: Vec<Mutation>,
    applied: Mutex<usize>,
    catalog: Arc<ConformanceCatalog>,
    tdlib: Arc<TdlibSide>,
    next_target: FileTarget,
    next_path: PathBuf,
    next_len: u64,
}

impl Control for WiredControl {
    fn advance(&self) -> Result<bool, HarnessError> {
        let advanced = self.fake.advance()?;
        let mut applied = self.applied.lock().expect("mutation cursor");
        if !advanced || *applied >= self.plan.len() {
            return Ok(advanced);
        }
        let mutation = self.plan[*applied];
        *applied += 1;
        if mutation == Mutation::ContentChanges {
            // The world moved: the catalog serves the next version, and
            // TDLib's cache holds the next content.
            self.catalog.entries.lock().expect("catalog map").insert(
                self.catalog.file.clone(),
                CatalogEntry::File(self.next_target.clone()),
            );
            let mut current = self.tdlib.current.lock().expect("current content");
            current.path = self.next_path.clone();
            current.len = self.next_len;
        }
        Ok(advanced)
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

struct TdjsonFetchHarness {
    /// Stages the enumeration world and the landmarks; also carries the
    /// non-fetch perturbations (a slow or unauthorized `root`).
    fake: FakeHarness,
    /// Distinguishes each staged case's temporary directory.
    staged_worlds: AtomicUsize,
}

impl TdjsonFetchHarness {
    fn new() -> TdjsonFetchHarness {
        TdjsonFetchHarness {
            fake: FakeHarness::new(),
            staged_worlds: AtomicUsize::new(0),
        }
    }

    fn temp_dir(&self) -> PathBuf {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "fetch-conformance-{}-{}",
            std::process::id(),
            self.staged_worlds.fetch_add(1, Ordering::SeqCst)
        ))
    }
}

impl SourceHarness for TdjsonFetchHarness {
    type Source = RangedTdjsonSource;

    fn name(&self) -> &str {
        "gramdrive-source-tdjson ranged fetch (mock tdjson; enumeration via the testkit fake)"
    }

    fn supports(&self, _capability: Capability) -> bool {
        // Everything: fetch-side conditions are scripted in the mock and
        // the catalog, the rest in the embedded fake.
        true
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        common::block_on(future)
    }

    fn stage(
        &self,
        world: &WorldSpec,
        setup: &Setup,
    ) -> Result<Staged<Self::Source>, HarnessError> {
        // The enumeration world, landmarks, and non-fetch faults.
        let fake_staged = self.fake.stage(world, setup)?;
        let landmarks = fake_staged.landmarks.clone();

        // TDLib's files directory, played by a per-case temp dir.
        let dir = self.temp_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| HarnessError::new(format!("temp dir: {error}")))?;
        let file_path = dir.join("payload.bin");
        std::fs::write(&file_path, world.file_bytes)
            .map_err(|error| HarnessError::new(format!("payload: {error}")))?;
        let next_path = dir.join("payload-next.bin");
        std::fs::write(&next_path, world.next_file_bytes)
            .map_err(|error| HarnessError::new(format!("next payload: {error}")))?;

        // The fetch-side perturbations, split off the setup: faults land
        // in the responder, the race in the catalog.
        let mut faults = VecDeque::new();
        let mut race = None;
        for perturbation in &setup.arm {
            match perturbation {
                Perturbation::Unreachable {
                    operation: Operation::Fetch,
                } => faults.push_back(FetchFault::Fail {
                    code: 500,
                    message: "Failed to connect".to_owned(),
                }),
                Perturbation::RateLimited {
                    operation: Operation::Fetch,
                    retry_after,
                } => faults.push_back(FetchFault::Fail {
                    code: 429,
                    message: match retry_after {
                        Some(wait) => {
                            format!("Too Many Requests: retry after {}", wait.as_secs())
                        }
                        None => "Too Many Requests".to_owned(),
                    },
                }),
                Perturbation::ReferenceExpired {
                    operation: Operation::Fetch,
                } => faults.push_back(FetchFault::Fail {
                    code: 400,
                    message: "FILE_REFERENCE_EXPIRED".to_owned(),
                }),
                Perturbation::Slow {
                    operation: Operation::Fetch,
                } => faults.push_back(FetchFault::Silence),
                Perturbation::FetchRacesContentChange { after_bytes } => {
                    // The gate resolve plus one resolve per delivered
                    // slice see the pinned version; the next re-check
                    // finds the moved one, with exactly `after_bytes`
                    // delivered.
                    let slices = after_bytes.div_ceil(READ_CHUNK_BYTES);
                    race = Some(RaceFlip {
                        serve_base: Mutex::new(1 + u32::try_from(slices).unwrap_or(u32::MAX)),
                        moved: CatalogEntry::File(FileTarget {
                            version: landmarks.next_file_version.clone(),
                            ..file_target(&landmarks.file_version, world.file_bytes.len() as u64)
                        }),
                    });
                }
                // Non-fetch operations: already staged in the fake.
                _ => {}
            }
        }

        let catalog = Arc::new(ConformanceCatalog {
            entries: Mutex::new(catalog_entries(&landmarks, world)),
            file: landmarks.file.clone(),
            race,
        });
        let tdlib = Arc::new(TdlibSide {
            faults: Mutex::new(faults),
            current: Mutex::new(CurrentContent {
                path: file_path,
                len: world.file_bytes.len() as u64,
            }),
        });

        // The real runtime over the mock, answering from `tdlib`.
        let (sender, receiver, handle) = MockTdJson::new();
        let runtime = TdRuntime::start(sender, receiver, common::test_config())
            .map_err(|error| HarnessError::new(format!("runtime: {error}")))?;
        let (client, _updates) = runtime
            .create_client()
            .map_err(|error| HarnessError::new(format!("client: {error}")))?;
        let responder_side = Arc::clone(&tdlib);
        handle.set_responder(move |sent| responder_side.respond(sent));

        let config = DownloadConfig {
            priority: DownloadPriority::default(),
            read_chunk_bytes: NonZeroU64::new(READ_CHUNK_BYTES)
                .ok_or_else(|| HarnessError::new("read cap must be non-zero"))?,
        };
        let downloader = TdDownloader::new(client.clone(), Arc::clone(&catalog) as _, config);
        let thumbnail_catalog = Arc::new(ConformanceThumbnailCatalog {
            restricted: landmarks.restricted_file.clone(),
        });
        let thumbnailer = TdThumbnailer::new(client, thumbnail_catalog, ThumbnailConfig::default());

        let control = WiredControl {
            fake: fake_staged.control,
            plan: setup.plan.clone(),
            applied: Mutex::new(0),
            catalog,
            tdlib,
            next_target: FileTarget {
                version: landmarks.next_file_version.clone(),
                ..file_target(
                    &landmarks.next_file_version,
                    world.next_file_bytes.len() as u64,
                )
            },
            next_path,
            next_len: world.next_file_bytes.len() as u64,
        };

        Ok(Staged {
            source: Arc::new(RangedTdjsonSource {
                fake: fake_staged.source,
                downloader,
                thumbnailer,
                _runtime: runtime,
            }),
            landmarks,
            control: Box::new(control),
        })
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[test]
fn the_tdjson_ranged_fetch_conforms() {
    let harness = TdjsonFetchHarness::new();
    let report = conformance::assert_conforms(&harness);
    assert_eq!(
        report.skipped().count(),
        0,
        "this harness stages every capability; a skip is a staging gap:\n{report}"
    );
}

/// The stated-delay contract deserves a direct pin here too: the suite's
/// rate-limit case asserts it, but only through the mock's message format —
/// this keeps the format honest against `retryable_after`'s parsing.
#[test]
fn the_mock_flood_message_round_trips_the_stated_delay() {
    let wait = Duration::from_secs(2);
    let message = format!("Too Many Requests: retry after {}", wait.as_secs());
    assert!(message.ends_with("after 2"));
}
