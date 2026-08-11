//! Non-viewing Telegram story normalization and discovery.
//!
//! [`StoryMachine`] is sans I/O: it emits one allow-listed metadata request at
//! a time and consumes its response plus the ordered update stream. It never
//! constructs `openStory`, `closeStory`, a live group-call request,
//! `downloadFile`, or a mutating story method. The normalized vocabulary has
//! no caption, content JSON, local path, or bytes. Save-permitted persistent
//! media retains only typed TDLib locator facts; protected stories are
//! redacted before they can cross the source boundary.

use std::collections::{BTreeSet, VecDeque};

use serde_json::{Value, json};

use crate::error::{TdError, retryable_after};

const STORY_PAGE_SIZE: i32 = 50;
const MAX_READY_COMMITS: usize = 256;
const MAX_ENRICHMENTS: usize = 256;

/// Requests that the background metadata dispatcher may submit.
pub const BACKGROUND_STORY_REQUESTS: &[&str] = &[
    "loadActiveStories",
    "getStory",
    "getChatActiveStories",
    "getChatPostedToChatPageStories",
    "getChatArchivedStories",
    "getChatMember",
];

/// Exact `getRemoteFile.file_type` constructor derived from a story role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryFileType {
    /// `photo.sizes[].photo`.
    PhotoStory,
    /// Primary or alternative `storyVideo.video`.
    VideoStory,
    /// File-backed story video thumbnail.
    Thumbnail,
}

impl StoryFileType {
    /// Stable TDJSON constructor name persisted beside the locator.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::PhotoStory => "fileTypePhotoStory",
            Self::VideoStory => "fileTypeVideoStory",
            Self::Thumbnail => "fileTypeThumbnail",
        }
    }
}

/// Byte-free locator for one save-permitted Telegram story representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryContentLocator {
    /// Deterministic representation role (`photo-size:<type>`,
    /// `video-primary`, `video-alternative`, or `video-thumbnail`).
    pub role: String,
    /// Exact role-derived rematerialization type.
    pub file_type: StoryFileType,
    /// Whether this is the canonical full-content source for the story blob.
    pub is_primary: bool,
    /// Current-session `file.id` hint.
    pub local_file_id: Option<i32>,
    /// Refreshable `file.remote.id`.
    pub remote_file_id: Option<String>,
    /// Stable `file.remote.unique_id`, when TDLib supplied it.
    pub remote_unique_id: Option<String>,
    /// Exact current size, when known.
    pub size: Option<u64>,
    /// Expected size is retained separately and never claimed as exact.
    pub expected_size: Option<u64>,
    /// Equality token for the represented bytes.
    pub content_version: String,
}

/// Returns whether a TDJSON request belongs to the reviewed, non-viewing
/// story metadata allow-list.
pub fn background_story_request_allowed(request: &Value) -> bool {
    request
        .get("@type")
        .and_then(Value::as_str)
        .is_some_and(|method| BACKGROUND_STORY_REQUESTS.contains(&method))
}

/// Account type relevant to owner archive eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryAccountKind {
    /// A regular user account may own its own story chat.
    Regular,
    /// Bot accounts are explicitly ineligible for the owner branch.
    Bot,
    /// A new/unknown account kind fails closed.
    Unsupported,
}

/// Chat type relevant to management-right lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryChatKind {
    /// One-to-one user chat.
    Private,
    /// Legacy basic group.
    Group,
    /// Supergroup without channel semantics.
    Supergroup,
    /// Broadcast channel.
    Channel,
}

/// Archive capability proven by exact account/member evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryArchiveCapability {
    /// Rights have not yet been resolved.
    Unknown,
    /// The current regular user owns this story chat.
    Owner,
    /// Exact membership rights permit editing stories.
    Manageable,
    /// The known account/chat/member shape does not grant archive access.
    Ineligible,
    /// The current account type cannot own archived stories.
    AccountUnsupported,
    /// TDLib did not provide enough rights evidence, so access fails closed.
    RightsUnavailable,
}

impl StoryArchiveCapability {
    /// Whether `getChatArchivedStories` is authorized.
    pub fn permits_archive(self) -> bool {
        matches!(self, Self::Owner | Self::Manageable)
    }
}

/// Privacy-safe normalized story content classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryContentKind {
    /// `storyInfo` identity is known and byte-free enrichment is pending.
    MetadataPending,
    /// Supported Telegram photo representation.
    Photo,
    /// Supported Telegram video representation.
    Video,
    /// Saving/forwarding is forbidden; all content metadata is redacted.
    Protected,
    /// Story content representation is not supported by the pinned TDLib contract.
    Unsupported,
    /// Live story exists but requires an explicit viewer lifecycle.
    LiveUnavailable,
}

/// One canonical, byte-free story observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryObservation {
    /// Canonical poster chat identifier.
    pub poster_chat_id: i64,
    /// Canonical story identifier within the poster chat.
    pub story_id: i64,
    /// Absolute Telegram publication timestamp in milliseconds.
    pub date_ms: i64,
    /// Whether the story has a persistent chat-profile appearance.
    pub is_posted_to_chat_page: bool,
    /// Whether Telegram permits later explicit saving/forwarding.
    pub can_be_forwarded: bool,
    /// Honest byte-free content classification.
    pub content_kind: StoryContentKind,
    /// MIME type for supported and save-permitted content only.
    pub mime_type: Option<String>,
    /// Reported size for supported and save-permitted content only.
    pub exact_size: Option<u64>,
    /// Stable version of the normalized metadata identity.
    pub content_version: String,
    /// Typed save-permitted locators. Empty for every unavailable/protected state.
    pub locators: Vec<StoryContentLocator>,
}

/// Durable cursors used to resume one chat without replay-order assumptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoryScanCursor {
    /// Whether authoritative active membership has been committed.
    pub active_complete: bool,
    /// Inclusive cursor for the next profile page.
    pub profile_cursor: Option<i32>,
    /// Reconciliation generation assigned by durable state.
    pub profile_scan_generation: u64,
    /// Whether the profile scan reached its current end.
    pub profile_complete: bool,
    /// Rights evidence controlling archived-story requests.
    pub archive_capability: StoryArchiveCapability,
    /// Inclusive cursor for the next archive page.
    pub archive_cursor: Option<i32>,
    /// Whether archive discovery is complete or known ineligible.
    pub archive_complete: bool,
}

/// One bounded chat scan fed to [`StoryMachine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoryChatPlan {
    /// Poster chat to scan.
    pub chat_id: i64,
    /// Chat shape used by the exact rights branch.
    pub chat_kind: StoryChatKind,
    /// Durable resume boundary.
    pub cursor: StoryScanCursor,
}

/// Transactional output of the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryCommit {
    /// One bounded `storyListMain` load call reached a durable boundary.
    ActiveListProgress {
        /// Whether TDLib returned the documented 404 exhaustion signal.
        complete: bool,
    },
    /// Complete active membership for one chat. Absence is authoritative.
    ActiveSnapshot {
        /// Poster chat whose active membership is authoritative.
        chat_id: i64,
        /// Telegram's opaque `chatActiveStories.order`, used only for the
        /// separate Stories chat-list presentation.
        order: i64,
        /// Byte-free active observations.
        stories: Vec<StoryObservation>,
    },
    /// Full non-viewing metadata for one canonical story.
    Upsert(StoryObservation),
    /// One profile page and its durable inclusive cursor.
    ProfilePage {
        /// Poster chat owning the profile page.
        chat_id: i64,
        /// Durable reconciliation generation for the page.
        generation: u64,
        /// Normalized page after inclusive-boundary deduplication.
        stories: Vec<StoryObservation>,
        /// First-page profile pin order; empty on continuation pages.
        pinned_story_ids: Vec<i64>,
        /// Inclusive cursor for the next page.
        next_from_story_id: Option<i32>,
        /// Whether the profile scan reached its current end.
        complete: bool,
    },
    /// Exact archive-right classification; only two variants grant requests.
    ArchiveCapability {
        /// Poster chat whose rights were classified.
        chat_id: i64,
        /// Exact rights result.
        capability: StoryArchiveCapability,
    },
    /// One rights-gated archive page and its durable inclusive cursor.
    ArchivePage {
        /// Poster chat owning the archived page.
        chat_id: i64,
        /// Normalized page after inclusive-boundary deduplication.
        stories: Vec<StoryObservation>,
        /// Inclusive cursor for the next page.
        next_from_story_id: Option<i32>,
        /// Whether archived backfill reached its current end.
        complete: bool,
    },
    /// TDLib made a story inaccessible without a reason code.
    Inaccessible {
        /// Poster chat of the inaccessible story.
        poster_chat_id: i64,
        /// Story identity; TDLib supplies no reliable reason code.
        story_id: i64,
    },
    /// Temporary posting identity was replaced by the server identity.
    PostSucceeded {
        /// Temporary local story identifier being replaced.
        old_story_id: i64,
        /// Canonical server observation.
        story: StoryObservation,
    },
    /// One chat scan reached a stable boundary.
    ScanComplete {
        /// Poster chat that reached a stable scan boundary.
        chat_id: i64,
    },
}

/// Retry advice for one unchanged metadata request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoryBackoff {
    /// Server-advised delay when TDLib supplied one.
    pub retry_after_secs: Option<u64>,
    /// One-based retry attempt count.
    pub attempt: u32,
}

/// One caller obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryStep {
    /// Submit exactly this allow-listed metadata request.
    Submit(Value),
    /// Wait before resubmitting the unchanged pending request.
    Backoff(StoryBackoff),
    /// Persist this output atomically with its durable cursor.
    Commit(StoryCommit),
    /// Update pressure exceeded bounded memory; rescan these chat ids.
    ResyncRequired(Vec<i64>),
    /// No work is currently available.
    Idle,
}

/// Invalid plan or wire shape. Durable cursors are the recovery path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryError {
    /// Caller supplied an invalid or conflicting scan plan.
    Plan {
        /// Privacy-safe diagnostic detail.
        detail: String,
    },
    /// TDLib returned a shape inconsistent with the pinned contract.
    Protocol {
        /// Privacy-safe diagnostic detail.
        detail: String,
    },
    /// TDLib rejected a metadata request.
    Request {
        /// Structured TDLib error used for retry classification.
        source: TdError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Active,
    Profile,
    Rights,
    Archive,
    Done,
}

struct NormalizedStoryPage {
    stories: Vec<StoryObservation>,
    pinned_story_ids: Vec<i64>,
    next_from_story_id: Option<i32>,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    ActiveList,
    Enrichment { chat_id: i64, story_id: i64 },
    Active,
    Profile,
    Rights,
    Archive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Enrichment {
    chat_id: i64,
    story_id: i64,
}

#[derive(Debug)]
struct ActiveJob {
    plan: StoryChatPlan,
    phase: Phase,
}

/// Bounded, one-request-at-a-time story discovery and live reducer.
#[derive(Debug)]
pub struct StoryMachine {
    current_user_id: i64,
    account_kind: StoryAccountKind,
    job: Option<ActiveJob>,
    pending: Option<(PendingKind, Value)>,
    pending_chat_id: Option<i64>,
    pending_invalidated: bool,
    backoff: Option<StoryBackoff>,
    retry_attempt: u32,
    ready: VecDeque<StoryCommit>,
    enrichments: VecDeque<Enrichment>,
    enrichment_keys: BTreeSet<(i64, i64)>,
    resync_chats: BTreeSet<i64>,
    active_list_loading: bool,
    active_list_complete: bool,
}

impl StoryMachine {
    /// Creates an idle machine after the owned session has resolved `getMe`.
    pub fn new(current_user_id: i64, account_kind: StoryAccountKind) -> Result<Self, StoryError> {
        if current_user_id <= 0 {
            return Err(StoryError::Plan {
                detail: "current_user_id must be positive".to_owned(),
            });
        }
        Ok(Self {
            current_user_id,
            account_kind,
            job: None,
            pending: None,
            pending_chat_id: None,
            pending_invalidated: false,
            backoff: None,
            retry_attempt: 0,
            ready: VecDeque::new(),
            enrichments: VecDeque::new(),
            enrichment_keys: BTreeSet::new(),
            resync_chats: BTreeSet::new(),
            active_list_loading: false,
            active_list_complete: false,
        })
    }

    /// Starts the session-global `storyListMain` loader. Each successful call
    /// is one bounded checkpoint; TDLib's 404 exhaustion signal ends the pass.
    /// A relaunch starts a new pass and therefore resumes TDLib's internally
    /// persisted list position without relying on ordinary chat worklists.
    pub fn start_active_list_discovery(&mut self) -> Result<(), StoryError> {
        if self.pending.is_some() {
            return Err(StoryError::Plan {
                detail: "active story list discovery must start without a pending request"
                    .to_owned(),
            });
        }
        self.active_list_loading = true;
        self.active_list_complete = false;
        Ok(())
    }

    /// Reports whether the session-global loader still owns the next story
    /// scheduling slot. Per-chat work must not be attached to a global loader
    /// request because its errors and progress have account scope.
    #[must_use]
    pub fn has_active_list_work(&self) -> bool {
        self.active_list_loading && !self.active_list_complete
    }

    /// Adds one bounded chat scan. The caller reads the next durable work item
    /// only after the previous scan reaches [`StoryCommit::ScanComplete`].
    pub fn enqueue_chat(&mut self, plan: StoryChatPlan) -> Result<(), StoryError> {
        if plan.chat_id == 0 {
            return Err(StoryError::Plan {
                detail: "story chat id must be non-zero".to_owned(),
            });
        }
        if self.job.is_some() {
            return Err(StoryError::Plan {
                detail: "a story chat scan is already active".to_owned(),
            });
        }
        let phase = if plan.cursor.active_complete {
            if plan.cursor.profile_complete {
                Phase::Rights
            } else {
                Phase::Profile
            }
        } else {
            Phase::Active
        };
        self.job = Some(ActiveJob { plan, phase });
        Ok(())
    }

    /// Whether a bounded chat scan is currently active.
    pub fn has_active_chat(&self) -> bool {
        self.job.is_some()
    }

    /// Drops the current scan after a chat-scoped failure. Live commits for
    /// other chats remain queued; durable progress owns retry/relaunch.
    pub fn abandon_active_chat(&mut self) -> Option<i64> {
        self.pending = None;
        self.pending_chat_id = None;
        self.pending_invalidated = false;
        self.backoff = None;
        self.retry_attempt = 0;
        self.job.take().map(|job| job.plan.chat_id)
    }

    /// Replaces the provisional identity after the owned session resolves
    /// `getMe`. Buffered updates are identity-independent and remain queued.
    pub fn set_account_identity(
        &mut self,
        current_user_id: i64,
        account_kind: StoryAccountKind,
    ) -> Result<(), StoryError> {
        if current_user_id <= 0 || self.job.is_some() || self.pending.is_some() {
            return Err(StoryError::Plan {
                detail: "story account identity can change only while idle".to_owned(),
            });
        }
        self.current_user_id = current_user_id;
        self.account_kind = account_kind;
        Ok(())
    }

    /// Folds one ordered TDLib update. Unknown and non-story updates are ignored.
    pub fn on_update(&mut self, update: &Value) {
        match update.get("@type").and_then(Value::as_str) {
            Some("updateChatActiveStories") => {
                let Some(active) = update.get("active_stories") else {
                    return;
                };
                if let Ok((chat_id, order, stories)) = normalize_active_snapshot(active) {
                    self.invalidate_pending(chat_id);
                    for story in &stories {
                        if story.content_kind == StoryContentKind::MetadataPending {
                            self.queue_enrichment(chat_id, story.story_id);
                        }
                    }
                    self.queue_commit(
                        chat_id,
                        StoryCommit::ActiveSnapshot {
                            chat_id,
                            order,
                            stories,
                        },
                    );
                }
            }
            Some("updateStory") => {
                if let Some(story) = update.get("story")
                    && let Ok(story) = normalize_story(story)
                {
                    self.invalidate_pending(story.poster_chat_id);
                    self.queue_commit(story.poster_chat_id, StoryCommit::Upsert(story));
                }
            }
            Some("updateStoryDeleted") => {
                let chat_id = update.get("story_poster_chat_id").and_then(Value::as_i64);
                let story_id = update.get("story_id").and_then(Value::as_i64);
                if let (Some(poster_chat_id), Some(story_id)) = (chat_id, story_id) {
                    self.invalidate_pending(poster_chat_id);
                    self.queue_commit(
                        poster_chat_id,
                        StoryCommit::Inaccessible {
                            poster_chat_id,
                            story_id,
                        },
                    );
                }
            }
            Some("updateStoryPostSucceeded") => {
                let old_story_id = update.get("old_story_id").and_then(Value::as_i64);
                let story = update
                    .get("story")
                    .and_then(|value| normalize_story(value).ok());
                if let (Some(old_story_id), Some(story)) = (old_story_id, story) {
                    self.invalidate_pending(story.poster_chat_id);
                    self.queue_commit(
                        story.poster_chat_id,
                        StoryCommit::PostSucceeded {
                            old_story_id,
                            story,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    /// Returns the next obligation. Every generated request is checked against
    /// the reviewed allow-list before it can leave the machine.
    pub fn next_step(&mut self) -> Result<StoryStep, StoryError> {
        if let Some(commit) = self.ready.pop_front() {
            return Ok(StoryStep::Commit(commit));
        }
        if !self.resync_chats.is_empty() {
            return Ok(StoryStep::ResyncRequired(
                std::mem::take(&mut self.resync_chats).into_iter().collect(),
            ));
        }
        if let Some(backoff) = self.backoff.take() {
            return Ok(StoryStep::Backoff(backoff));
        }
        if let Some((_, request)) = &self.pending {
            return Ok(StoryStep::Submit(request.clone()));
        }
        if let Some(enrichment) = self.enrichments.pop_front() {
            self.enrichment_keys
                .remove(&(enrichment.chat_id, enrichment.story_id));
            return self.submit(
                PendingKind::Enrichment {
                    chat_id: enrichment.chat_id,
                    story_id: enrichment.story_id,
                },
                json!({
                    "@type": "getStory",
                    "story_poster_chat_id": enrichment.chat_id,
                    "story_id": enrichment.story_id,
                    "only_local": false
                }),
            );
        }
        if self.active_list_loading && !self.active_list_complete {
            return self.submit(
                PendingKind::ActiveList,
                json!({
                    "@type": "loadActiveStories",
                    "story_list": {"@type": "storyListMain"}
                }),
            );
        }
        let Some(job) = &self.job else {
            return Ok(StoryStep::Idle);
        };
        match job.phase {
            Phase::Active => self.submit(
                PendingKind::Active,
                json!({"@type": "getChatActiveStories", "chat_id": job.plan.chat_id}),
            ),
            Phase::Profile => self.submit(
                PendingKind::Profile,
                json!({
                    "@type": "getChatPostedToChatPageStories",
                    "chat_id": job.plan.chat_id,
                    "from_story_id": job.plan.cursor.profile_cursor.unwrap_or(0),
                    "limit": STORY_PAGE_SIZE
                }),
            ),
            Phase::Rights => self.next_rights_step(),
            Phase::Archive => self.submit(
                PendingKind::Archive,
                json!({
                    "@type": "getChatArchivedStories",
                    "chat_id": job.plan.chat_id,
                    "from_story_id": job.plan.cursor.archive_cursor.unwrap_or(0),
                    "limit": STORY_PAGE_SIZE
                }),
            ),
            Phase::Done => {
                let chat_id = job.plan.chat_id;
                self.job = None;
                Ok(StoryStep::Commit(StoryCommit::ScanComplete { chat_id }))
            }
        }
    }

    /// Feeds the outcome of the current submitted request.
    pub fn on_response(&mut self, outcome: Result<Value, TdError>) -> Result<(), StoryError> {
        let Some((kind, request)) = self.pending.take() else {
            return Err(StoryError::Protocol {
                detail: "story response arrived without a pending request".to_owned(),
            });
        };
        let request_chat_id = self.pending_chat_id.take();
        let invalidated = std::mem::take(&mut self.pending_invalidated);
        if !matches!(kind, PendingKind::Rights | PendingKind::ActiveList) && invalidated {
            // An ordered live update for this chat was observed after the
            // request was submitted. The response may describe older truth;
            // discard it and force the composing transaction layer to reset
            // durable cursors before retrying from a fresh boundary.
            self.backoff = None;
            self.retry_attempt = 0;
            self.resync_chats
                .insert(request_chat_id.ok_or_else(|| StoryError::Protocol {
                    detail: "chat request lost its update-order checkpoint".to_owned(),
                })?);
            return Ok(());
        }
        match outcome {
            Ok(value) => {
                self.retry_attempt = 0;
                self.accept_response(kind, value)
            }
            Err(error) => {
                if let Some(retry_after_secs) = retryable_after(&error) {
                    self.retry_attempt = self.retry_attempt.saturating_add(1);
                    self.backoff = Some(StoryBackoff {
                        retry_after_secs,
                        attempt: self.retry_attempt,
                    });
                    self.pending = Some((kind, request));
                    self.pending_chat_id = request_chat_id;
                    return Ok(());
                }
                if kind == PendingKind::ActiveList && matches!(error, TdError::Td { code: 404, .. })
                {
                    self.active_list_complete = true;
                    self.queue_commit(0, StoryCommit::ActiveListProgress { complete: true });
                    return Ok(());
                }
                if matches!(kind, PendingKind::Enrichment { .. })
                    && matches!(error, TdError::Td { code: 404, .. })
                {
                    return Ok(());
                }
                if kind == PendingKind::Rights {
                    let chat_id = self.active_chat_id()?;
                    self.queue_commit(
                        chat_id,
                        StoryCommit::ArchiveCapability {
                            chat_id,
                            capability: StoryArchiveCapability::RightsUnavailable,
                        },
                    );
                    self.set_capability(StoryArchiveCapability::RightsUnavailable)?;
                    self.set_phase(Phase::Done)?;
                    return Ok(());
                }
                Err(StoryError::Request { source: error })
            }
        }
    }

    fn accept_response(&mut self, kind: PendingKind, value: Value) -> Result<(), StoryError> {
        match kind {
            PendingKind::ActiveList => {
                if value.get("@type").and_then(Value::as_str) != Some("ok") {
                    return Err(StoryError::Protocol {
                        detail: "loadActiveStories response is not ok".to_owned(),
                    });
                }
                self.queue_commit(0, StoryCommit::ActiveListProgress { complete: false });
            }
            PendingKind::Enrichment { .. } => {
                let story = normalize_story(&value)?;
                self.queue_commit(story.poster_chat_id, StoryCommit::Upsert(story));
            }
            PendingKind::Active => {
                let (chat_id, order, stories) = normalize_active_snapshot(&value)?;
                if chat_id != self.active_chat_id()? {
                    return Err(StoryError::Protocol {
                        detail: "active story response belongs to another chat".to_owned(),
                    });
                }
                for story in &stories {
                    if story.content_kind == StoryContentKind::MetadataPending {
                        self.queue_enrichment(chat_id, story.story_id);
                    }
                }
                self.queue_commit(
                    chat_id,
                    StoryCommit::ActiveSnapshot {
                        chat_id,
                        order,
                        stories,
                    },
                );
                self.set_active_complete(true)?;
                self.set_phase(Phase::Profile)?;
            }
            PendingKind::Profile => self.accept_story_page(value, true)?,
            PendingKind::Rights => {
                let capability = classify_chat_member(&value, self.current_user_id)?;
                let chat_id = self.active_chat_id()?;
                self.queue_commit(
                    chat_id,
                    StoryCommit::ArchiveCapability {
                        chat_id,
                        capability,
                    },
                );
                self.set_capability(capability)?;
                self.set_phase(if capability.permits_archive() {
                    Phase::Archive
                } else {
                    Phase::Done
                })?;
            }
            PendingKind::Archive => self.accept_story_page(value, false)?,
        }
        Ok(())
    }

    fn accept_story_page(&mut self, value: Value, profile: bool) -> Result<(), StoryError> {
        let job = self.job.as_ref().ok_or_else(|| StoryError::Protocol {
            detail: "story page arrived without an active chat".to_owned(),
        })?;
        let chat_id = job.plan.chat_id;
        let from = if profile {
            job.plan.cursor.profile_cursor
        } else {
            job.plan.cursor.archive_cursor
        };
        let page = normalize_story_page(&value, chat_id, from, profile)?;
        if profile {
            let generation = job.plan.cursor.profile_scan_generation;
            self.queue_commit(
                chat_id,
                StoryCommit::ProfilePage {
                    chat_id,
                    generation,
                    stories: page.stories,
                    pinned_story_ids: page.pinned_story_ids,
                    next_from_story_id: page.next_from_story_id,
                    complete: page.complete,
                },
            );
            let Some(job) = self.job.as_mut() else {
                return Err(StoryError::Protocol {
                    detail: "profile page lost its active chat".to_owned(),
                });
            };
            job.plan.cursor.profile_cursor = page.next_from_story_id;
            job.plan.cursor.profile_complete = page.complete;
            if page.complete {
                job.phase = Phase::Rights;
            }
        } else {
            self.queue_commit(
                chat_id,
                StoryCommit::ArchivePage {
                    chat_id,
                    stories: page.stories,
                    next_from_story_id: page.next_from_story_id,
                    complete: page.complete,
                },
            );
            let Some(job) = self.job.as_mut() else {
                return Err(StoryError::Protocol {
                    detail: "archive page lost its active chat".to_owned(),
                });
            };
            job.plan.cursor.archive_cursor = page.next_from_story_id;
            job.plan.cursor.archive_complete = page.complete;
            if page.complete {
                job.phase = Phase::Done;
            }
        }
        Ok(())
    }

    fn next_rights_step(&mut self) -> Result<StoryStep, StoryError> {
        let job = self.job.as_ref().ok_or_else(|| StoryError::Protocol {
            detail: "rights step without an active chat".to_owned(),
        })?;
        if job.plan.cursor.archive_complete {
            self.set_phase(Phase::Done)?;
            return self.next_step();
        }
        if job.plan.cursor.archive_capability.permits_archive() {
            self.set_phase(Phase::Archive)?;
            return self.next_step();
        }
        let capability = if self.account_kind != StoryAccountKind::Regular {
            Some(StoryArchiveCapability::AccountUnsupported)
        } else if job.plan.chat_id == self.current_user_id {
            Some(StoryArchiveCapability::Owner)
        } else if matches!(
            job.plan.chat_kind,
            StoryChatKind::Private | StoryChatKind::Group
        ) {
            Some(StoryArchiveCapability::Ineligible)
        } else {
            None
        };
        if let Some(capability) = capability {
            let chat_id = job.plan.chat_id;
            self.queue_commit(
                chat_id,
                StoryCommit::ArchiveCapability {
                    chat_id,
                    capability,
                },
            );
            self.set_capability(capability)?;
            self.set_phase(if capability.permits_archive() {
                Phase::Archive
            } else {
                Phase::Done
            })?;
            return self.next_step();
        }
        self.submit(
            PendingKind::Rights,
            json!({
                "@type": "getChatMember",
                "chat_id": job.plan.chat_id,
                "member_id": {
                    "@type": "messageSenderUser",
                    "user_id": self.current_user_id
                }
            }),
        )
    }

    fn submit(&mut self, kind: PendingKind, request: Value) -> Result<StoryStep, StoryError> {
        if !background_story_request_allowed(&request) {
            return Err(StoryError::Protocol {
                detail: format!(
                    "background story request is not allow-listed: {}",
                    request
                        .get("@type")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>")
                ),
            });
        }
        let chat_id = match kind {
            PendingKind::ActiveList => None,
            PendingKind::Enrichment { chat_id, .. } => Some(chat_id),
            _ => Some(self.active_chat_id()?),
        };
        self.pending = Some((kind, request.clone()));
        self.pending_chat_id = chat_id;
        self.pending_invalidated = false;
        Ok(StoryStep::Submit(request))
    }

    fn queue_enrichment(&mut self, chat_id: i64, story_id: i64) {
        let key = (chat_id, story_id);
        if self.enrichment_keys.contains(&key) {
            return;
        }
        if self.enrichments.len() >= MAX_ENRICHMENTS {
            self.resync_chats.insert(chat_id);
            return;
        }
        self.enrichment_keys.insert(key);
        self.enrichments.push_back(Enrichment { chat_id, story_id });
    }

    fn queue_commit(&mut self, chat_id: i64, commit: StoryCommit) {
        if self.ready.len() >= MAX_READY_COMMITS {
            if chat_id != 0 {
                self.resync_chats.insert(chat_id);
            }
            return;
        }
        self.ready.push_back(commit);
    }

    fn invalidate_pending(&mut self, chat_id: i64) {
        if self.pending_chat_id == Some(chat_id) {
            self.pending_invalidated = true;
        }
    }

    fn active_chat_id(&self) -> Result<i64, StoryError> {
        self.job
            .as_ref()
            .map(|job| job.plan.chat_id)
            .ok_or_else(|| StoryError::Protocol {
                detail: "story request has no active chat".to_owned(),
            })
    }

    fn set_phase(&mut self, phase: Phase) -> Result<(), StoryError> {
        self.job
            .as_mut()
            .ok_or_else(|| StoryError::Protocol {
                detail: "story phase has no active chat".to_owned(),
            })?
            .phase = phase;
        Ok(())
    }

    fn set_active_complete(&mut self, complete: bool) -> Result<(), StoryError> {
        self.job
            .as_mut()
            .ok_or_else(|| StoryError::Protocol {
                detail: "story cursor has no active chat".to_owned(),
            })?
            .plan
            .cursor
            .active_complete = complete;
        Ok(())
    }

    fn set_capability(&mut self, capability: StoryArchiveCapability) -> Result<(), StoryError> {
        self.job
            .as_mut()
            .ok_or_else(|| StoryError::Protocol {
                detail: "story capability has no active chat".to_owned(),
            })?
            .plan
            .cursor
            .archive_capability = capability;
        Ok(())
    }
}

/// Parses exact `getMe` evidence without guessing a future user type.
pub fn normalize_story_account(value: &Value) -> Result<(i64, StoryAccountKind), StoryError> {
    if value.get("@type").and_then(Value::as_str) != Some("user") {
        return Err(StoryError::Protocol {
            detail: "getMe response is not user".to_owned(),
        });
    }
    let id = value
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| StoryError::Protocol {
            detail: "getMe user is missing id".to_owned(),
        })?;
    let kind = match value
        .get("type")
        .and_then(|kind| kind.get("@type"))
        .and_then(Value::as_str)
    {
        Some("userTypeRegular") => StoryAccountKind::Regular,
        Some("userTypeBot") => StoryAccountKind::Bot,
        _ => StoryAccountKind::Unsupported,
    };
    Ok((id, kind))
}

fn normalize_active_snapshot(
    value: &Value,
) -> Result<(i64, i64, Vec<StoryObservation>), StoryError> {
    if value.get("@type").and_then(Value::as_str) != Some("chatActiveStories") {
        return Err(StoryError::Protocol {
            detail: "active story response is not chatActiveStories".to_owned(),
        });
    }
    let chat_id = value
        .get("chat_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| StoryError::Protocol {
            detail: "chatActiveStories is missing chat_id".to_owned(),
        })?;
    let order = value
        .get("order")
        .and_then(Value::as_i64)
        .ok_or_else(|| StoryError::Protocol {
            detail: "chatActiveStories is missing order".to_owned(),
        })?;
    let values = value
        .get("stories")
        .and_then(Value::as_array)
        .ok_or_else(|| StoryError::Protocol {
            detail: "chatActiveStories is missing stories".to_owned(),
        })?;
    let mut seen = BTreeSet::new();
    let mut stories = Vec::with_capacity(values.len());
    for value in values {
        let story_id = value
            .get("story_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| StoryError::Protocol {
                detail: "storyInfo is missing story_id".to_owned(),
            })?;
        if !seen.insert(story_id) {
            return Err(StoryError::Protocol {
                detail: "chatActiveStories contains duplicate story id".to_owned(),
            });
        }
        let date_ms = seconds_to_ms(value.get("date").and_then(Value::as_i64))?;
        let content_kind = if value.get("is_live").and_then(Value::as_bool) == Some(true) {
            StoryContentKind::LiveUnavailable
        } else {
            StoryContentKind::MetadataPending
        };
        stories.push(StoryObservation {
            poster_chat_id: chat_id,
            story_id,
            date_ms,
            is_posted_to_chat_page: false,
            can_be_forwarded: false,
            content_kind,
            mime_type: None,
            exact_size: None,
            content_version: story_version(chat_id, story_id, date_ms, content_kind, None),
            locators: Vec::new(),
        });
    }
    Ok((chat_id, order, stories))
}

/// Normalizes one full TDLib `story` object without opening it or changing
/// viewed state. This is also used by the download adapter after a stale
/// story file reference: `getStory(..., only_local = false)` refreshes the
/// same canonical metadata and the caller verifies identity before retrying.
pub fn normalize_story(value: &Value) -> Result<StoryObservation, StoryError> {
    if value.get("@type").and_then(Value::as_str) != Some("story") {
        return Err(StoryError::Protocol {
            detail: "story response is not story".to_owned(),
        });
    }
    let poster_chat_id = value
        .get("poster_chat_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| StoryError::Protocol {
            detail: "story is missing poster_chat_id".to_owned(),
        })?;
    let story_id = value
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| StoryError::Protocol {
            detail: "story is missing id".to_owned(),
        })?;
    let date_ms = seconds_to_ms(value.get("date").and_then(Value::as_i64))?;
    let can_be_forwarded = value
        .get("can_be_forwarded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (content_kind, mime_type, exact_size, locators) = if !can_be_forwarded {
        (StoryContentKind::Protected, None, None, Vec::new())
    } else {
        normalize_allowed_content(poster_chat_id, story_id, date_ms, value.get("content"))
    };
    let content_version = locators
        .iter()
        .find(|locator| locator.is_primary)
        .map_or_else(
            || story_version(poster_chat_id, story_id, date_ms, content_kind, exact_size),
            |locator| locator.content_version.clone(),
        );
    Ok(StoryObservation {
        poster_chat_id,
        story_id,
        date_ms,
        is_posted_to_chat_page: value
            .get("is_posted_to_chat_page")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        can_be_forwarded,
        content_kind,
        mime_type,
        exact_size,
        content_version,
        locators,
    })
}

fn normalize_allowed_content(
    chat_id: i64,
    story_id: i64,
    date_ms: i64,
    content: Option<&Value>,
) -> (
    StoryContentKind,
    Option<String>,
    Option<u64>,
    Vec<StoryContentLocator>,
) {
    match content
        .and_then(|content| content.get("@type"))
        .and_then(Value::as_str)
    {
        Some("storyContentPhoto") => {
            let mut locators = content
                .and_then(|content| content.get("photo"))
                .and_then(|photo| photo.get("sizes"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|size| {
                    let role = format!(
                        "photo-size:{}",
                        size.get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    );
                    normalize_locator(
                        chat_id,
                        story_id,
                        date_ms,
                        role,
                        StoryFileType::PhotoStory,
                        size.get("photo")?,
                    )
                })
                .collect::<Vec<_>>();
            let primary = locators
                .iter()
                .enumerate()
                .max_by_key(|(_, locator)| {
                    (
                        locator.size.or(locator.expected_size).unwrap_or(0),
                        &locator.role,
                    )
                })
                .map(|(index, _)| index);
            if let Some(primary) = primary {
                locators[primary].is_primary = true;
                let exact_size = locators[primary].size;
                (
                    StoryContentKind::Photo,
                    Some("image/jpeg".to_owned()),
                    exact_size,
                    locators,
                )
            } else {
                (StoryContentKind::Unsupported, None, None, Vec::new())
            }
        }
        Some("storyContentVideo") => {
            let primary_video = content
                .and_then(|content| content.get("video"))
                .and_then(|video| video.get("video"));
            let Some(mut primary) = primary_video.and_then(|file| {
                normalize_locator(
                    chat_id,
                    story_id,
                    date_ms,
                    "video-primary".to_owned(),
                    StoryFileType::VideoStory,
                    file,
                )
            }) else {
                return (StoryContentKind::Unsupported, None, None, Vec::new());
            };
            primary.is_primary = true;
            let exact_size = primary.size;
            let mut locators = vec![primary];
            if let Some(alternative) = content
                .and_then(|content| content.get("alternative_video"))
                .and_then(|video| video.get("video"))
                .and_then(|file| {
                    normalize_locator(
                        chat_id,
                        story_id,
                        date_ms,
                        "video-alternative".to_owned(),
                        StoryFileType::VideoStory,
                        file,
                    )
                })
            {
                locators.push(alternative);
            }
            if let Some(thumbnail) = content
                .and_then(|content| content.get("video"))
                .and_then(|video| video.get("thumbnail"))
                .and_then(|thumbnail| thumbnail.get("file"))
                .and_then(|file| {
                    normalize_locator(
                        chat_id,
                        story_id,
                        date_ms,
                        "video-thumbnail".to_owned(),
                        StoryFileType::Thumbnail,
                        file,
                    )
                })
            {
                locators.push(thumbnail);
            }
            (
                StoryContentKind::Video,
                Some("video/mp4".to_owned()),
                exact_size,
                locators,
            )
        }
        Some("storyContentLive") => (StoryContentKind::LiveUnavailable, None, None, Vec::new()),
        _ => (StoryContentKind::Unsupported, None, None, Vec::new()),
    }
}

fn normalize_locator(
    chat_id: i64,
    story_id: i64,
    date_ms: i64,
    role: String,
    file_type: StoryFileType,
    file: &Value,
) -> Option<StoryContentLocator> {
    if file.get("@type").and_then(Value::as_str) != Some("file") {
        return None;
    }
    let local_file_id = file
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id != 0);
    let remote = file.get("remote");
    let remote_file_id = nonempty_string(remote.and_then(|remote| remote.get("id")));
    let remote_unique_id = nonempty_string(remote.and_then(|remote| remote.get("unique_id")));
    if local_file_id.is_none() && remote_file_id.is_none() {
        return None;
    }
    let size = positive_u64(file.get("size"));
    let expected_size = positive_u64(file.get("expected_size"));
    let content_version = locator_version(
        chat_id,
        story_id,
        date_ms,
        &role,
        local_file_id,
        remote_file_id.as_deref(),
        remote_unique_id.as_deref(),
        size,
        expected_size,
    );
    Some(StoryContentLocator {
        role,
        file_type,
        is_primary: false,
        local_file_id,
        remote_file_id,
        remote_unique_id,
        size,
        expected_size,
        content_version,
    })
}

fn nonempty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn positive_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}

fn normalize_story_page(
    value: &Value,
    chat_id: i64,
    from: Option<i32>,
    profile: bool,
) -> Result<NormalizedStoryPage, StoryError> {
    if value.get("@type").and_then(Value::as_str) != Some("stories") {
        return Err(StoryError::Protocol {
            detail: "story page response is not stories".to_owned(),
        });
    }
    let values = value
        .get("stories")
        .and_then(Value::as_array)
        .ok_or_else(|| StoryError::Protocol {
            detail: "stories response is missing stories".to_owned(),
        })?;
    let mut stories = Vec::with_capacity(values.len());
    let mut seen = BTreeSet::new();
    let mut last_returned_story_id = None;
    for value in values {
        let story = normalize_story(value)?;
        if story.poster_chat_id != chat_id {
            return Err(StoryError::Protocol {
                detail: "story page contains another chat".to_owned(),
            });
        }
        let story_id = i32::try_from(story.story_id).map_err(|_| StoryError::Protocol {
            detail: "story id exceeds TDLib int32".to_owned(),
        })?;
        last_returned_story_id = Some(story_id);
        if Some(story_id) == from || !seen.insert(story.story_id) {
            continue;
        }
        stories.push(story);
    }
    // TDLib may return fewer objects than requested for performance. The raw
    // page's last id is the inclusive cursor even when that object was removed
    // from the normalized observations as the boundary duplicate. Exhaustion
    // is proven only by an empty page or a cursor that failed to decrease.
    let next = last_returned_story_id;
    let complete = values.is_empty()
        || from.is_some_and(|from_story_id| {
            next.is_none_or(|next_story_id| next_story_id >= from_story_id)
        });
    let pinned_story_ids = if profile && from.is_none() {
        let values = value
            .get("pinned_story_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| StoryError::Protocol {
                detail: "initial profile page is missing pinned_story_ids".to_owned(),
            })?;
        let mut seen = BTreeSet::new();
        values
            .iter()
            .map(|value| {
                let story_id = value.as_i64().ok_or_else(|| StoryError::Protocol {
                    detail: "pinned story id is not an integer".to_owned(),
                })?;
                if !seen.insert(story_id) {
                    return Err(StoryError::Protocol {
                        detail: "profile page contains duplicate pinned story id".to_owned(),
                    });
                }
                Ok(story_id)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(NormalizedStoryPage {
        stories,
        pinned_story_ids,
        next_from_story_id: next,
        complete,
    })
}

fn classify_chat_member(
    value: &Value,
    current_user_id: i64,
) -> Result<StoryArchiveCapability, StoryError> {
    if value.get("@type").and_then(Value::as_str) != Some("chatMember") {
        return Err(StoryError::Protocol {
            detail: "getChatMember response is not chatMember".to_owned(),
        });
    }
    let member_id = value.get("member_id").ok_or_else(|| StoryError::Protocol {
        detail: "chatMember is missing member_id".to_owned(),
    })?;
    if member_id.get("@type").and_then(Value::as_str) != Some("messageSenderUser")
        || member_id.get("user_id").and_then(Value::as_i64) != Some(current_user_id)
    {
        return Ok(StoryArchiveCapability::RightsUnavailable);
    }
    let status = value.get("status").ok_or_else(|| StoryError::Protocol {
        detail: "chatMember is missing status".to_owned(),
    })?;
    match status.get("@type").and_then(Value::as_str) {
        Some("chatMemberStatusCreator") => Ok(StoryArchiveCapability::Manageable),
        Some("chatMemberStatusAdministrator")
            if status
                .get("rights")
                .and_then(|rights| rights.get("can_edit_stories"))
                .and_then(Value::as_bool)
                == Some(true) =>
        {
            Ok(StoryArchiveCapability::Manageable)
        }
        _ => Ok(StoryArchiveCapability::Ineligible),
    }
}

fn seconds_to_ms(seconds: Option<i64>) -> Result<i64, StoryError> {
    seconds
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or_else(|| StoryError::Protocol {
            detail: "story date is missing or overflows milliseconds".to_owned(),
        })
}

#[allow(clippy::too_many_arguments)]
fn locator_version(
    chat_id: i64,
    story_id: i64,
    date_ms: i64,
    role: &str,
    local_file_id: Option<i32>,
    remote_file_id: Option<&str>,
    remote_unique_id: Option<&str>,
    size: Option<u64>,
    expected_size: Option<u64>,
) -> String {
    // ContentVersion is an equality token, not a cryptographic content hash.
    // Two fixed FNV-1a lanes keep untrusted TDLib identifiers bounded while
    // retaining deterministic identity across process relaunches.
    let stable_identity = remote_unique_id
        .map(|value| format!("unique:{value}"))
        .or_else(|| remote_file_id.map(|value| format!("remote:{value}")))
        .unwrap_or_else(|| format!("local:{}", local_file_id.unwrap_or_default()));
    let input = format!(
        "{chat_id}\0{story_id}\0{date_ms}\0{role}\0{stable_identity}\0{}\0{}",
        size.unwrap_or(0),
        expected_size.unwrap_or(0)
    );
    let left = fnv1a64(input.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let right = fnv1a64(input.as_bytes(), 0x8422_2325_cbf2_9ce4);
    format!("story/{chat_id}/{story_id}/{role}/{left:016x}{right:016x}")
}

fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn story_version(
    chat_id: i64,
    story_id: i64,
    date_ms: i64,
    kind: StoryContentKind,
    size: Option<u64>,
) -> String {
    format!(
        "story/{chat_id}/{story_id}/{date_ms}/{kind:?}/{}",
        size.unwrap_or(0)
    )
}
