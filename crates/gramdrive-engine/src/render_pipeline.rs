//! Atomic composition and publication of one bounded monthly document pair.
//!
//! A state read transaction supplies [`MonthRenderSnapshot`]: both formats are
//! composed from that one message/event set and watermark. Bytes are written
//! beneath a new immutable version directory and the directory is renamed into
//! place before one SQLite transaction publishes every appearance, render
//! watermark, cache locator, and provider change-journal row. A crash before
//! the transaction leaves only unreachable staged bytes; a crash after it sees
//! the complete pair.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use gramdrive_model::hash::sha256;
use gramdrive_model::identity::{
    AppearanceKey, CanonicalKey, ContentHash, DocFormat, DocPartition, ItemKey,
};
use gramdrive_model::version::{ContentVersion, InvalidVersionToken, MetadataVersion};
use gramdrive_render::chat_json::{self, ChatKind, ChatMetadataInput};
use gramdrive_render::markdown::{
    self, Deletion, DisplayTimeZone, InvalidDisplayTimeZone, MarkdownInput, MessageBody,
    MessageHistory, RetentionMode, Revision, Sender,
};
use gramdrive_render::ndjson::{self, MessagesInput};
use gramdrive_state::repo::{
    CacheEntryRecord, CacheKind, CacheVerification, ChatRecord, ChatType, FileFacts,
    MessageEventKind, MessagePayload, MonthRenderSnapshot, RenderCatalogEntry, RenderOutput,
    RetentionMode as StateRetentionMode,
};
use gramdrive_state::{StateError, StateStore};

static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);

/// Process-local claims held by native hydration while File Provider clones a
/// published generated document. Publication is durable, but the final clone
/// is necessarily outside SQLite; this narrow lease bridges that hand-off so
/// replacing a cache row cannot reclaim the old immutable generation midway
/// through the clone. A process crash drops every claim, after which normal
/// publication or startup reconciliation reclaims the orphan.
static GENERATED_FILE_LEASES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn generated_file_leases() -> &'static Mutex<HashMap<PathBuf, usize>> {
    GENERATED_FILE_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keeps one generated document materialization alive across a native
/// hand-off. Dropping the returned lease releases the claim.
#[derive(Debug)]
#[must_use = "the lease must live until the native host has cloned the file"]
pub struct GeneratedFileLease {
    path: PathBuf,
}

impl GeneratedFileLease {
    /// Attempts to claim an existing generated file. The existence check and
    /// claim share the reclamation lock, so a publication sweep cannot remove
    /// the file between them.
    pub fn acquire(path: &Path) -> Option<Self> {
        let mut leases = generated_file_leases()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !path.is_file() {
            return None;
        }
        let path = path.to_path_buf();
        *leases.entry(path.clone()).or_default() += 1;
        Some(Self { path })
    }

    /// The managed file protected by this lease.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for GeneratedFileLease {
    fn drop(&mut self) {
        let mut leases = generated_file_leases()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(count) = leases.get_mut(&self.path) else {
            return;
        };
        if *count == 1 {
            leases.remove(&self.path);
        } else {
            *count -= 1;
        }
    }
}

#[cfg(test)]
type ReclaimBeforeUnlinkHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

/// Test-only barrier for pinning the exact point at which reclamation owns the
/// lease mutex and has decided that an old path is no longer database-claimed.
/// Production has no callback at this boundary; the hook exists solely to
/// prove that a concurrent native hand-off cannot acquire a lease before the
/// subsequent unlink.
#[cfg(test)]
static RECLAIM_BEFORE_UNLINK_HOOK: OnceLock<Mutex<Option<ReclaimBeforeUnlinkHook>>> =
    OnceLock::new();

#[cfg(test)]
fn reclaim_before_unlink_hook() -> &'static Mutex<Option<ReclaimBeforeUnlinkHook>> {
    RECLAIM_BEFORE_UNLINK_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn notify_reclaim_before_unlink(path: &Path) {
    let hook = reclaim_before_unlink_hook()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

/// A normalized payload decoded into the renderer's provider-neutral shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRevision {
    /// Source edit time carried by this revision.
    pub edited_at_ms: Option<i64>,
    /// Full normalized content snapshot.
    pub body: MessageBody,
}

/// Decodes the versioned normalized payload stored in `message_events`.
pub trait MessagePayloadDecoder {
    /// Decoder-specific error, converted to a privacy-safe pipeline category.
    type Error: fmt::Display;

    /// Decodes one retained payload.
    fn decode(&self, payload: &MessagePayload) -> Result<DecodedRevision, Self::Error>;
}

/// Deterministic bytes and provenance for one monthly pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMonth {
    /// Shared chat/month identity.
    pub partition: DocPartition,
    /// Shared state input watermark.
    pub input_watermark_seq: i64,
    /// Account policy generation pinned with the normalized snapshot.
    pub render_generation: i64,
    /// Persisted timezone that shaped the civil partition and output.
    pub display_timezone: String,
    /// Retention projection that shaped visible revisions.
    pub retention_mode: StateRetentionMode,
    /// UTC lower bound used for state selection and race checks.
    pub start_ms: i64,
    /// UTC exclusive upper bound.
    pub end_ms: i64,
    /// Human-readable bytes.
    pub markdown: Vec<u8>,
    /// Structured bytes.
    pub ndjson: Vec<u8>,
}

impl RenderedMonth {
    fn bytes(&self, format: DocFormat) -> Option<&[u8]> {
        match format {
            DocFormat::Markdown => Some(&self.markdown),
            DocFormat::Ndjson => Some(&self.ndjson),
            DocFormat::Json => None,
        }
    }

    fn content_version(&self, format: DocFormat) -> Result<ContentVersion, RenderPipelineError> {
        let retention = render_retention(self.retention_mode);
        let token = match format {
            DocFormat::Markdown => markdown::content_version_token(
                self.input_watermark_seq,
                self.render_generation,
                retention,
                &self.display_timezone,
            ),
            DocFormat::Ndjson => ndjson::content_version_token(
                self.input_watermark_seq,
                self.render_generation,
                retention,
                &self.display_timezone,
            ),
            DocFormat::Json => return Err(RenderPipelineError::UnsupportedFormat),
        };
        ContentVersion::new(token).map_err(RenderPipelineError::Version)
    }
}

/// Stable materialized locations of a staged monthly pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedMonth {
    /// Version directory atomically renamed into place.
    pub directory: PathBuf,
    /// Markdown materialization path.
    pub markdown: PathBuf,
    /// NDJSON materialization path.
    pub ndjson: PathBuf,
}

/// Result of the atomic database publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonthPublication {
    /// `true` when no newer event affecting this month raced composition.
    pub clean: bool,
    /// Number of provider appearances updated across both formats.
    pub published_items: usize,
}

/// Deterministic `.chat.json` bytes pinned to one canonical chat row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedChatMetadata {
    /// Canonical metadata snapshot that shaped the bytes.
    pub source: ChatRecord,
    /// Compact JSON bytes, including one trailing newline.
    pub bytes: Vec<u8>,
    /// Stable pin derived from the exact bytes.
    pub content_version: ContentVersion,
}

/// Stable materialized location of one chat-metadata generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChatMetadata {
    /// Immutable version directory atomically renamed into place.
    pub directory: PathBuf,
    /// Materialized JSON file.
    pub path: PathBuf,
}

/// Result of one atomic `.chat.json` publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatMetadataPublication {
    /// Number of live provider appearances updated.
    pub published_items: usize,
}

/// Composes privacy-bounded chat metadata from one canonical state row.
pub fn compose_chat_metadata(
    source: &ChatRecord,
) -> Result<RenderedChatMetadata, RenderPipelineError> {
    let kind = match source.chat_type {
        ChatType::Private => ChatKind::Private,
        ChatType::Group => ChatKind::Group,
        ChatType::Supergroup => ChatKind::Supergroup,
        ChatType::Channel => ChatKind::Channel,
    };
    let bytes = chat_json::render(&ChatMetadataInput {
        kind,
        title: &source.title,
        username: source.username.as_deref(),
        is_protected: source.is_protected,
        archive_mode: source.archive_mode,
        left_at_ms: source.left_at_ms,
        deleted_at_ms: source.deleted_at_ms,
        last_update_at_ms: source.last_update_at_ms,
    })
    .into_bytes();
    let content_version = ContentVersion::new(chat_json::content_version_token(&bytes))
        .map_err(RenderPipelineError::Version)?;
    Ok(RenderedChatMetadata {
        source: source.clone(),
        bytes,
        content_version,
    })
}

/// Writes one immutable chat-metadata generation beneath the managed cache.
pub fn stage_chat_metadata(
    cache_root: &Path,
    rendered: &RenderedChatMetadata,
) -> Result<StagedChatMetadata, RenderPipelineError> {
    let chat = rendered.source.key;
    let base = cache_root
        .join("generated")
        .join(chat.scope.account.account_id.0.to_string())
        .join(chat.scope.namespace_version.0.to_string())
        .join(chat.chat_id.0.to_string());
    fs::create_dir_all(&base)?;
    let digest = content_hash_hex(&sha256(&rendered.bytes));
    let final_dir = base.join(format!(
        "chat-json-s{}-r{}-{digest}",
        chat_json::SCHEMA_VERSION,
        chat_json::RENDERER_VERSION
    ));
    let path = final_dir.join("chat.json");
    if final_dir.exists() {
        if fs::read(&path)? == rendered.bytes {
            return Ok(StagedChatMetadata {
                directory: final_dir,
                path,
            });
        }
        return Err(RenderPipelineError::VersionCollision);
    }
    let stage_id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
    let stage = base.join(format!(".chat-stage-{}-{stage_id}", std::process::id()));
    fs::create_dir(&stage)?;
    let result = (|| -> Result<(), std::io::Error> {
        write_synced(&stage.join("chat.json"), &rendered.bytes)?;
        fs::rename(&stage, &final_dir)?;
        OpenOptions::new().read(true).open(&base)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(RenderPipelineError::Io(error));
    }
    Ok(StagedChatMetadata {
        directory: final_dir,
        path,
    })
}

/// Atomically publishes `.chat.json` into every live chat-list appearance.
pub fn publish_chat_metadata(
    store: &mut StateStore,
    rendered: &RenderedChatMetadata,
    staged: &StagedChatMetadata,
    published_at_ms: i64,
) -> Result<ChatMetadataPublication, RenderPipelineError> {
    if fs::read(&staged.path)? != rendered.bytes {
        return Err(RenderPipelineError::StagedContentMismatch);
    }
    let content_hash = sha256(&rendered.bytes);
    let logical_size =
        u64::try_from(rendered.bytes.len()).map_err(|_| RenderPipelineError::SizeOverflow {
            size: rendered.bytes.len(),
        })?;
    let digest = content_hash_hex(&content_hash);
    let metadata_version = MetadataVersion::new(format!(
        "chat-json-s{}-r{}-{digest}",
        chat_json::SCHEMA_VERSION,
        chat_json::RENDERER_VERSION
    ))
    .map_err(RenderPipelineError::MetadataVersion)?;
    let txn = store.write_txn()?;
    let current = txn
        .read()
        .chat(&rendered.source.key)?
        .ok_or(StateError::RowNotFound { entity: "chat" })?;
    if current != rendered.source {
        return Err(RenderPipelineError::MetadataChanged);
    }
    let catalog = txn.read().chat_render_catalog(rendered.source.key)?;
    validate_chat_catalog(&catalog, rendered)?;
    let mut published_items = 0usize;
    for entry in catalog {
        let existing = txn
            .read()
            .item(&entry.item)?
            .ok_or(StateError::RowNotFound { entity: "item" })?;
        let expected = existing
            .content
            .as_ref()
            .and_then(|facts| facts.content_version.as_ref());
        let modified_at_ms = if expected == Some(&rendered.content_version) {
            existing.modified_at_ms.unwrap_or(published_at_ms)
        } else {
            published_at_ms
        };
        txn.ensure_render_state(
            &entry.item,
            chat_json::RENDERER_VERSION,
            chat_json::SCHEMA_VERSION,
        )?;
        txn.update_item_content(
            &entry.item,
            expected,
            &FileFacts {
                mime_type: Some("application/json".to_owned()),
                logical_size: Some(logical_size),
                content_version: Some(rendered.content_version.clone()),
            },
            &metadata_version,
            modified_at_ms,
        )?;
        // The document's size just changed, and that size is a term of its
        // chat directory's published rollup (BUG-260728-2qfzbd). Bounded by
        // tree depth and each ancestor's child count — never a chat-wide
        // reconciliation.
        txn.refresh_ancestor_rollups(&entry.item)?;
        txn.publish_static_render(
            &entry.item,
            &RenderOutput {
                content_version: rendered.content_version.clone(),
                content_hash: Some(content_hash),
                logical_size,
            },
            modified_at_ms,
        )?;
        let existing_cache = txn.read().cache_entry(&entry.item)?;
        txn.upsert_cache_entry(&CacheEntryRecord {
            item: entry.item,
            account: rendered.source.key.scope.account,
            content_version: rendered.content_version.clone(),
            kind: CacheKind::GeneratedDoc,
            size: logical_size,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: existing_cache.as_ref().and_then(|cache| cache.pin),
            last_access_at_ms: existing_cache
                .as_ref()
                .map_or(published_at_ms, |cache| cache.last_access_at_ms),
            materialized_at_ms: modified_at_ms,
            materialization_ref: Some(staged.path.to_string_lossy().into_owned()),
        })?;
        published_items = published_items.saturating_add(1);
    }
    txn.commit()?;
    reclaim_unreferenced_generations(
        store,
        staged
            .directory
            .parent()
            .ok_or_else(|| std::io::Error::other("chat generation has no managed parent"))?,
    )?;
    Ok(ChatMetadataPublication { published_items })
}

fn content_hash_hex(hash: &ContentHash) -> String {
    let ContentHash::Sha256(bytes) = hash;
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_chat_catalog(
    catalog: &[RenderCatalogEntry],
    rendered: &RenderedChatMetadata,
) -> Result<(), RenderPipelineError> {
    let mut views = std::collections::HashSet::new();
    for entry in catalog {
        let ItemKey::Appearance(AppearanceKey {
            view,
            item: CanonicalKey::GeneratedDoc(document),
        }) = entry.item.key()
        else {
            return Err(RenderPipelineError::IncompleteCatalog);
        };
        if view != entry.view
            || document.chat != rendered.source.key
            || document.partition != DocPartition::Chat
            || document.format != DocFormat::Json
            || document.schema_family != chat_json::CHAT_SCHEMA_FAMILY
            || entry.format != DocFormat::Json
            || entry.schema_family != chat_json::CHAT_SCHEMA_FAMILY
            || !views.insert(entry.view)
        {
            return Err(RenderPipelineError::IncompleteCatalog);
        }
    }
    if views.is_empty() {
        return Err(RenderPipelineError::IncompleteCatalog);
    }
    Ok(())
}

/// Composes Markdown and NDJSON from exactly one state snapshot.
pub fn compose_month<D: MessagePayloadDecoder>(
    snapshot: &MonthRenderSnapshot,
    year: u16,
    month: u8,
    decoder: &D,
) -> Result<RenderedMonth, RenderPipelineError> {
    let timezone = DisplayTimeZone::named(&snapshot.display_timezone)?;
    let partition = DocPartition::Month { year, month };
    let (expected_start_ms, expected_end_ms) = timezone.month_bounds_ms(year, month)?;
    if snapshot.start_ms != expected_start_ms || snapshot.end_ms != expected_end_ms {
        return Err(RenderPipelineError::ProvenanceMismatch);
    }
    let mode = render_retention(snapshot.retention_mode);
    let mut messages = Vec::with_capacity(snapshot.messages.len());
    for stored in &snapshot.messages {
        let mut revisions = Vec::new();
        let mut deletion = None;
        for event in &stored.events {
            match event.kind {
                MessageEventKind::Observed | MessageEventKind::Edited => {
                    let Some(payload) = &event.payload else {
                        continue;
                    };
                    let decoded =
                        decoder
                            .decode(payload)
                            .map_err(|error| RenderPipelineError::Decode {
                                event_seq: event.event_seq,
                                detail: error.to_string(),
                            })?;
                    revisions.push(Revision {
                        event_seq: event.event_seq,
                        edited_at_ms: decoded.edited_at_ms,
                        observed_at_ms: event.observed_at_ms,
                        payload_schema: payload.schema,
                        body: decoded.body,
                    });
                }
                MessageEventKind::Deleted => {
                    deletion = Some(Deletion {
                        observed_at_ms: event.observed_at_ms,
                    });
                }
            }
        }
        messages.push(MessageHistory {
            message_id: stored.message_id,
            sender: stored.sender_id.map(|id| Sender { id }),
            sent_at_ms: stored.sent_at_ms,
            revisions,
            deletion,
        });
    }
    let markdown = markdown::render_transcript(&MarkdownInput {
        chat: snapshot.chat,
        partition,
        retention_mode: mode,
        timezone: &timezone,
        input_watermark_seq: snapshot.input_watermark_seq,
        render_generation: snapshot.render_generation,
        messages: &messages,
    })
    .into_bytes();
    let ndjson = ndjson::render_messages(&MessagesInput {
        chat: snapshot.chat,
        partition,
        retention_mode: mode,
        display_timezone: timezone.label(),
        input_watermark_seq: snapshot.input_watermark_seq,
        render_generation: snapshot.render_generation,
        messages: &messages,
    })
    .into_bytes();
    Ok(RenderedMonth {
        partition,
        input_watermark_seq: snapshot.input_watermark_seq,
        render_generation: snapshot.render_generation,
        display_timezone: snapshot.display_timezone.clone(),
        retention_mode: snapshot.retention_mode,
        start_ms: snapshot.start_ms,
        end_ms: snapshot.end_ms,
        markdown,
        ndjson,
    })
}

fn render_retention(mode: StateRetentionMode) -> RetentionMode {
    match mode {
        StateRetentionMode::Mirror => RetentionMode::Mirror,
        StateRetentionMode::Audit => RetentionMode::Audit,
    }
}

/// Writes both files into one immutable version directory and atomically
/// renames that directory into the managed cache namespace.
pub fn stage_month(
    cache_root: &Path,
    snapshot: &MonthRenderSnapshot,
    rendered: &RenderedMonth,
) -> Result<StagedMonth, RenderPipelineError> {
    let DocPartition::Month { year, month } = rendered.partition else {
        return Err(RenderPipelineError::InvalidPartition);
    };
    let base = cache_root
        .join("generated")
        .join(snapshot.chat.scope.account.account_id.0.to_string())
        .join(snapshot.chat.scope.namespace_version.0.to_string())
        .join(snapshot.chat.chat_id.0.to_string())
        .join(format!("{year:04}-{month:02}"));
    fs::create_dir_all(&base)?;
    let version_name = format!(
        "md-s{}-r{}-nd-s{}-r{}-g{}-w{}",
        markdown::SCHEMA_VERSION,
        markdown::RENDERER_VERSION,
        ndjson::SCHEMA_VERSION,
        ndjson::RENDERER_VERSION,
        rendered.render_generation,
        rendered.input_watermark_seq
    );
    let final_dir = base.join(version_name);
    let markdown = final_dir.join("Messages.md");
    let ndjson = final_dir.join("Messages.ndjson");
    if final_dir.exists() {
        if fs::read(&markdown)? == rendered.markdown && fs::read(&ndjson)? == rendered.ndjson {
            return Ok(StagedMonth {
                directory: final_dir,
                markdown,
                ndjson,
            });
        }
        return Err(RenderPipelineError::VersionCollision);
    }
    let stage_id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
    let stage = base.join(format!(".stage-{}-{stage_id}", std::process::id()));
    fs::create_dir(&stage)?;
    let result = (|| -> Result<(), std::io::Error> {
        write_synced(&stage.join("Messages.md"), &rendered.markdown)?;
        write_synced(&stage.join("Messages.ndjson"), &rendered.ndjson)?;
        fs::rename(&stage, &final_dir)?;
        OpenOptions::new().read(true).open(&base)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(RenderPipelineError::Io(error));
    }
    Ok(StagedMonth {
        directory: final_dir,
        markdown,
        ndjson,
    })
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Removes immutable generated files only after no cache row claims them.
///
/// Publication commits the new references first. A crash before this sweep
/// leaves harmless orphans for startup reconciliation; a live successful
/// publication does not accumulate generations until the next relaunch.
pub fn reclaim_unreferenced_generations(
    store: &mut StateStore,
    base: &Path,
) -> Result<(), RenderPipelineError> {
    let mut changed = false;
    for generation in fs::read_dir(base)? {
        let generation = generation?;
        if !generation.file_type()?.is_dir()
            || generation
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        for entry in fs::read_dir(generation.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || !entry.file_name().to_str().is_some_and(|name| {
                    matches!(
                        name,
                        "Messages.md" | "Messages.ndjson" | ".chat.json" | "chat.json"
                    )
                })
            {
                continue;
            }
            let path = entry.path();
            // Hold this mutex through the database check and unlink. A
            // concurrent hydration's `GeneratedFileLease::acquire` takes the
            // same mutex, so exactly one side wins: a lease keeps the path
            // alive for File Provider, or reclaim removes it before hydration
            // can expose it as a staged path.
            let leases = generated_file_leases()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if leases.contains_key(&path) {
                continue;
            }
            let reference = path.to_string_lossy();
            let claimed = store
                .read_txn()?
                .cache_reference_claimed(reference.as_ref())?;
            if !claimed {
                #[cfg(test)]
                notify_reclaim_before_unlink(&path);
                fs::remove_file(&path)?;
                changed = true;
            }
            drop(leases);
        }
        if fs::remove_dir(generation.path()).is_ok() {
            changed = true;
        }
    }
    if changed {
        OpenOptions::new().read(true).open(base)?.sync_all()?;
    }
    Ok(())
}

/// Atomically publishes every appearance of both monthly documents.
pub fn publish_month(
    store: &mut StateStore,
    snapshot: &MonthRenderSnapshot,
    rendered: &RenderedMonth,
    staged: &StagedMonth,
    published_at_ms: i64,
) -> Result<MonthPublication, RenderPipelineError> {
    validate_rendered_inputs(snapshot, rendered, staged)?;
    let metadata_version = MetadataVersion::new(format!(
        "monthly-render-v2-g{}-w{}",
        rendered.render_generation, rendered.input_watermark_seq
    ))
    .map_err(RenderPipelineError::MetadataVersion)?;
    let txn = store.write_txn()?;
    let DocPartition::Month { year, month } = rendered.partition else {
        return Err(RenderPipelineError::InvalidPartition);
    };
    let current_account = txn
        .read()
        .account(snapshot.chat.scope.account)?
        .ok_or(StateError::RowNotFound { entity: "account" })?;
    let current_generation = txn
        .read()
        .render_generation(snapshot.chat.scope.account)?
        .ok_or(StateError::RowNotFound { entity: "account" })?;
    if current_generation != rendered.render_generation
        || current_account.display_timezone != rendered.display_timezone
        || current_account.retention_mode != rendered.retention_mode
    {
        return Err(RenderPipelineError::PolicyChanged);
    }
    // Load the complete live appearance set inside the publication
    // transaction. A caller cannot accidentally publish one complete view
    // while leaving another live view stale.
    let catalog = txn
        .read()
        .month_render_catalog(snapshot.chat, year, month)?;
    validate_catalog(&catalog, snapshot, rendered)?;
    let mut clean = true;
    let mut published_items = 0usize;
    for entry in &catalog {
        let bytes = rendered
            .bytes(entry.format)
            .ok_or(RenderPipelineError::UnsupportedFormat)?;
        let content_version = rendered.content_version(entry.format)?;
        let content_hash = sha256(bytes);
        let logical_size = u64::try_from(bytes.len())
            .map_err(|_| RenderPipelineError::SizeOverflow { size: bytes.len() })?;
        let path = match entry.format {
            DocFormat::Markdown => &staged.markdown,
            DocFormat::Ndjson => &staged.ndjson,
            DocFormat::Json => return Err(RenderPipelineError::UnsupportedFormat),
        };
        let existing = txn
            .read()
            .item(&entry.item)?
            .ok_or(StateError::RowNotFound { entity: "item" })?;
        let expected = existing
            .content
            .as_ref()
            .and_then(|facts| facts.content_version.as_ref());
        let modified_at_ms = if expected == Some(&content_version) {
            existing.modified_at_ms.unwrap_or(published_at_ms)
        } else {
            published_at_ms
        };
        let class = match entry.format {
            DocFormat::Markdown => crate::render_plan::DocClass::MarkdownMonth,
            DocFormat::Ndjson => crate::render_plan::DocClass::NdjsonMonth,
            DocFormat::Json => return Err(RenderPipelineError::UnsupportedFormat),
        };
        txn.ensure_render_state(
            &entry.item,
            class.renderer_version(),
            class.schema_version(),
        )?;
        txn.update_item_content(
            &entry.item,
            expected,
            &FileFacts {
                mime_type: Some(match entry.format {
                    DocFormat::Markdown => "text/markdown".to_owned(),
                    DocFormat::Ndjson => "application/x-ndjson".to_owned(),
                    DocFormat::Json => return Err(RenderPipelineError::UnsupportedFormat),
                }),
                logical_size: Some(logical_size),
                content_version: Some(content_version.clone()),
            },
            &metadata_version,
            modified_at_ms,
        )?;
        // Same rollup obligation as the chat-metadata document above: the
        // month directory and its chat both publish a size this write moved.
        txn.refresh_ancestor_rollups(&entry.item)?;
        let outcome = txn.publish_month_render(
            &entry.item,
            &snapshot.chat,
            rendered.input_watermark_seq,
            rendered.start_ms..rendered.end_ms,
            &RenderOutput {
                content_version: content_version.clone(),
                content_hash: Some(content_hash),
                logical_size,
            },
            modified_at_ms,
        )?;
        clean &= outcome.clean;
        let existing_cache = txn.read().cache_entry(&entry.item)?;
        txn.upsert_cache_entry(&CacheEntryRecord {
            item: entry.item.clone(),
            account: snapshot.chat.scope.account,
            content_version,
            kind: CacheKind::GeneratedDoc,
            size: logical_size,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: existing_cache.as_ref().and_then(|cache| cache.pin),
            last_access_at_ms: existing_cache
                .as_ref()
                .map_or(published_at_ms, |cache| cache.last_access_at_ms),
            materialized_at_ms: modified_at_ms,
            materialization_ref: Some(path.to_string_lossy().into_owned()),
        })?;
        published_items = published_items.saturating_add(1);
    }
    txn.commit()?;
    reclaim_unreferenced_generations(
        store,
        staged
            .directory
            .parent()
            .ok_or_else(|| std::io::Error::other("month generation has no managed parent"))?,
    )?;
    Ok(MonthPublication {
        clean,
        published_items,
    })
}

fn validate_rendered_inputs(
    snapshot: &MonthRenderSnapshot,
    rendered: &RenderedMonth,
    staged: &StagedMonth,
) -> Result<(), RenderPipelineError> {
    if snapshot.input_watermark_seq != rendered.input_watermark_seq
        || snapshot.render_generation != rendered.render_generation
        || snapshot.display_timezone != rendered.display_timezone
        || snapshot.retention_mode != rendered.retention_mode
        || snapshot.start_ms != rendered.start_ms
        || snapshot.end_ms != rendered.end_ms
    {
        return Err(RenderPipelineError::ProvenanceMismatch);
    }
    if fs::read(&staged.markdown)? != rendered.markdown
        || fs::read(&staged.ndjson)? != rendered.ndjson
    {
        return Err(RenderPipelineError::StagedContentMismatch);
    }
    Ok(())
}

fn validate_catalog(
    catalog: &[RenderCatalogEntry],
    snapshot: &MonthRenderSnapshot,
    rendered: &RenderedMonth,
) -> Result<(), RenderPipelineError> {
    let mut views = HashMap::new();
    for entry in catalog {
        let ItemKey::Appearance(AppearanceKey {
            view,
            item: CanonicalKey::GeneratedDoc(document),
        }) = entry.item.key()
        else {
            return Err(RenderPipelineError::IncompleteCatalog);
        };
        if view != entry.view
            || document.chat != snapshot.chat
            || document.partition != rendered.partition
            || document.format != entry.format
            || document.schema_family != entry.schema_family
        {
            return Err(RenderPipelineError::IncompleteCatalog);
        }
        let (slot, expected_family) = match entry.format {
            DocFormat::Markdown => (0, markdown::MONTH_MARKDOWN_SCHEMA_FAMILY),
            DocFormat::Ndjson => (1, ndjson::MESSAGES_SCHEMA_FAMILY),
            DocFormat::Json => return Err(RenderPipelineError::UnsupportedFormat),
        };
        if entry.schema_family != expected_family {
            return Err(RenderPipelineError::IncompleteCatalog);
        }
        let formats = views.entry(entry.view).or_insert([false; 2]);
        if formats[slot] {
            return Err(RenderPipelineError::IncompleteCatalog);
        }
        formats[slot] = true;
    }
    if views.is_empty() || views.values().any(|formats| !formats[0] || !formats[1]) {
        return Err(RenderPipelineError::IncompleteCatalog);
    }
    Ok(())
}

/// Why monthly composition or publication failed.
#[derive(Debug)]
pub enum RenderPipelineError {
    /// State repository failure.
    State(StateError),
    /// Filesystem staging failure.
    Io(std::io::Error),
    /// Content version token was invalid.
    Version(InvalidVersionToken),
    /// Metadata version token was invalid.
    MetadataVersion(InvalidVersionToken),
    /// Persisted timezone or civil-month bounds were invalid.
    TimeZone(InvalidDisplayTimeZone),
    /// A normalized payload could not be decoded.
    Decode {
        /// Event whose normalized payload failed decoding.
        event_seq: i64,
        /// Decoder category/detail without payload bytes.
        detail: String,
    },
    /// A non-month partition reached the monthly pipeline.
    InvalidPartition,
    /// A non-monthly format reached the monthly pipeline.
    UnsupportedFormat,
    /// Existing bytes under a deterministic version path differ.
    VersionCollision,
    /// Snapshot and rendered provenance do not describe the same input.
    ProvenanceMismatch,
    /// Account render policy changed after the pinned snapshot was read.
    PolicyChanged,
    /// Canonical chat metadata changed after composition.
    MetadataChanged,
    /// The staged files do not contain the rendered pair being published.
    StagedContentMismatch,
    /// A chat-list appearance was missing or duplicated one monthly format.
    IncompleteCatalog,
    /// A generated file does not fit the persistent `u64` size field.
    SizeOverflow {
        /// Host-size value that could not be represented.
        size: usize,
    },
}

impl From<StateError> for RenderPipelineError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

impl From<std::io::Error> for RenderPipelineError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<InvalidDisplayTimeZone> for RenderPipelineError {
    fn from(error: InvalidDisplayTimeZone) -> Self {
        Self::TimeZone(error)
    }
}

impl fmt::Display for RenderPipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(error) => write!(formatter, "render publication state error: {error}"),
            Self::Io(error) => write!(formatter, "render publication I/O error: {error}"),
            Self::Version(error) | Self::MetadataVersion(error) => {
                write!(formatter, "render publication version error: {error}")
            }
            Self::TimeZone(error) => {
                write!(formatter, "render publication timezone error: {error}")
            }
            Self::Decode { event_seq, detail } => {
                write!(
                    formatter,
                    "normalized event {event_seq} could not be decoded: {detail}"
                )
            }
            Self::InvalidPartition => {
                formatter.write_str("monthly render requires a month partition")
            }
            Self::UnsupportedFormat => {
                formatter.write_str("monthly render supports Markdown and NDJSON only")
            }
            Self::VersionCollision => {
                formatter.write_str("render version path contains different bytes")
            }
            Self::ProvenanceMismatch => {
                formatter.write_str("snapshot and rendered month provenance differ")
            }
            Self::PolicyChanged => {
                formatter.write_str("account render policy changed after the month snapshot")
            }
            Self::MetadataChanged => {
                formatter.write_str("chat metadata changed after the JSON snapshot")
            }
            Self::StagedContentMismatch => {
                formatter.write_str("staged files differ from rendered month bytes")
            }
            Self::IncompleteCatalog => formatter.write_str(
                "every monthly appearance must contain exactly one Markdown/NDJSON pair",
            ),
            Self::SizeOverflow { size } => {
                write!(formatter, "rendered file size {size} does not fit u64")
            }
        }
    }
}

impl std::error::Error for RenderPipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::State(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Version(error) | Self::MetadataVersion(error) => Some(error),
            Self::TimeZone(error) => Some(error),
            Self::Decode { .. }
            | Self::InvalidPartition
            | Self::UnsupportedFormat
            | Self::VersionCollision
            | Self::ProvenanceMismatch
            | Self::PolicyChanged
            | Self::MetadataChanged
            | Self::StagedContentMismatch
            | Self::IncompleteCatalog
            | Self::SizeOverflow { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use gramdrive_state::StateStore;

    use super::{GeneratedFileLease, reclaim_before_unlink_hook, reclaim_unreferenced_generations};

    #[test]
    fn reclaim_cannot_unlink_between_lease_acquisition_and_native_hand_off() {
        let base = std::env::temp_dir().join(format!(
            "gramdrive-reclaim-lease-race-{}",
            std::process::id()
        ));
        let generation = base.join("obsolete-generation");
        let stale = generation.join("Messages.md");
        fs::create_dir_all(&generation).expect("generation directory");
        fs::write(&stale, b"exact staged bytes").expect("staged bytes");

        let (reclaim_arrived, reclaim_arrived_wait) = mpsc::sync_channel(0);
        let (reclaim_release, reclaim_release_wait) = mpsc::sync_channel(0);
        let stale_for_hook = stale.clone();
        let reclaim_release_wait = Arc::new(Mutex::new(reclaim_release_wait));
        *reclaim_before_unlink_hook()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(move |path| {
            if path == stale_for_hook {
                reclaim_arrived
                    .send(())
                    .expect("announce reclamation owns the lease mutex");
                reclaim_release_wait
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv()
                    .expect("release reclamation");
            }
        }));

        let reclaim_base = base.clone();
        let reclaim = thread::spawn(move || {
            let mut store = StateStore::open_in_memory().expect("state store");
            reclaim_unreferenced_generations(&mut store, &reclaim_base).expect("reclaim");
        });
        reclaim_arrived_wait
            .recv_timeout(Duration::from_secs(1))
            .expect("reclaim must reach the protected pre-unlink boundary");

        let stale_for_hydration = stale.clone();
        let (acquire_result, acquire_result_wait) = mpsc::sync_channel(1);
        let hydration = thread::spawn(move || {
            let lease = GeneratedFileLease::acquire(&stale_for_hydration);
            acquire_result
                .send(lease.is_some())
                .expect("report hydration lease result");
        });
        assert!(
            acquire_result_wait
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "hydration must wait while reclaim owns the decision and unlink"
        );

        reclaim_release.send(()).expect("release reclaim");
        reclaim.join().expect("reclaim thread");
        assert!(
            !acquire_result_wait
                .recv_timeout(Duration::from_secs(1))
                .expect("hydration result after reclaim"),
            "when reclaim wins, hydration receives a miss instead of a vanished path"
        );
        hydration.join().expect("hydration thread");
        assert!(
            !stale.exists(),
            "the obsolete generation was removed before another lease could be acquired"
        );

        *reclaim_before_unlink_hook()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        fs::remove_dir_all(&base).expect("temporary generation cleanup");
    }
}
