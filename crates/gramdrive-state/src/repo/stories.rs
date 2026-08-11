//! Canonical stories and byte-free active/month appearances.

use std::collections::BTreeSet;

use gramdrive_model::identity::{
    ChatId, ChatKey, ContentHash, StoryAppearanceLocation, StoryId, StoryKey,
};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{
    AttachmentAvailability, ReadTxn, RetentionMode, WriteTxn, hash_columns, hash_from_columns,
    scope_columns, size_from_column, size_to_column,
};

/// Honest durable state of story content metadata. The story repository never
/// stores caption text, content JSON, a cache path, or bytes; typed locators
/// exist only for save-permitted available content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryContentState {
    /// `storyInfo` was observed and a non-viewing `getStory` enrichment is pending.
    MetadataPending,
    /// Supported, accessible, save-permitted Telegram representation metadata.
    Available,
    /// Telegram says saving/forwarding is forbidden.
    Protected,
    /// The pinned TDLib reports an unsupported story representation.
    Unsupported,
    /// A live story was observed; the explicit viewer lifecycle is unavailable.
    LiveUnavailable,
    /// TDLib reported the canonical story inaccessible with no reliable cause.
    Inaccessible,
}

impl StoryContentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::MetadataPending => "metadata_pending",
            Self::Available => "available",
            Self::Protected => "protected",
            Self::Unsupported => "unsupported",
            Self::LiveUnavailable => "live_unavailable",
            Self::Inaccessible => "inaccessible",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "metadata_pending" => Ok(Self::MetadataPending),
            "available" => Ok(Self::Available),
            "protected" => Ok(Self::Protected),
            "unsupported" => Ok(Self::Unsupported),
            "live_unavailable" => Ok(Self::LiveUnavailable),
            "inaccessible" => Ok(Self::Inaccessible),
            other => Err(StateError::CorruptRow {
                table: "stories",
                detail: format!("unknown content_state '{other}'"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Refreshable metadata of one canonical Telegram story.
pub struct StoryFacts {
    /// Canonical `(poster_chat_id, story_id)` identity.
    pub key: StoryKey,
    /// Absolute Telegram timestamp. Display timezone conversion is separate.
    pub source_timestamp_ms: i64,
    /// MIME type of the Telegram representation, when known.
    pub mime_type: Option<String>,
    /// Exact reported byte size, when known.
    pub exact_size: Option<u64>,
    /// Version under which story content may be hydrated.
    pub content_version: ContentVersion,
    /// Current content availability.
    pub availability: AttachmentAvailability,
    /// Whether Telegram permits saving/forwarding the story bytes.
    pub can_be_forwarded: bool,
    /// Why story content is or is not available to later explicit hydration.
    pub content_state: StoryContentState,
}

/// Exact `getRemoteFile.file_type` constructor for a persisted story role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryLocatorFileType {
    /// `photo.sizes[].photo`.
    PhotoStory,
    /// Primary or alternative story video.
    VideoStory,
    /// File-backed story preview.
    Thumbnail,
}

impl StoryLocatorFileType {
    /// Stable TDJSON constructor name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PhotoStory => "fileTypePhotoStory",
            Self::VideoStory => "fileTypeVideoStory",
            Self::Thumbnail => "fileTypeThumbnail",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "fileTypePhotoStory" => Ok(Self::PhotoStory),
            "fileTypeVideoStory" => Ok(Self::VideoStory),
            "fileTypeThumbnail" => Ok(Self::Thumbnail),
            other => Err(StateError::CorruptRow {
                table: "story_content_locators",
                detail: format!("unknown file_type '{other}'"),
            }),
        }
    }
}

/// Byte-free TDLib locator for one canonical story representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryContentLocatorRecord {
    /// Canonical story identity.
    pub story: StoryKey,
    /// Deterministic representation role.
    pub role: String,
    /// Exact rematerialization type derived from the role.
    pub file_type: StoryLocatorFileType,
    /// Whether this locator is the one canonical full-content/blob source.
    pub is_primary: bool,
    /// Current-session `file.id` hint.
    pub local_file_id: Option<i32>,
    /// Refreshable TDLib remote file ID.
    pub remote_file_id: Option<String>,
    /// Stable TDLib remote unique ID, when present.
    pub remote_unique_id: Option<String>,
    /// Exact current size, when known.
    pub size: Option<u64>,
    /// Expected size, retained separately from exact size.
    pub expected_size: Option<u64>,
    /// Equality token for the represented bytes.
    pub content_version: ContentVersion,
}

/// Stored canonical story facts and the optional verified-byte link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryState {
    /// Refreshable story metadata.
    pub facts: StoryFacts,
    /// Hash of the one canonical byte object, when materialized.
    pub blob_hash: Option<ContentHash>,
    /// Time the linked bytes were last verified.
    pub last_verified_at_ms: Option<i64>,
    /// Time TDLib reported the story inaccessible, without inventing a cause.
    pub inaccessible_at_ms: Option<i64>,
    /// Typed byte-free locators. Empty unless content is available and permitted.
    pub locators: Vec<StoryContentLocatorRecord>,
}

/// Byte-free placement of a canonical story in the virtual tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryAppearanceRecord {
    /// Canonical story being presented.
    pub story: StoryKey,
    /// Ephemeral active or persistent monthly placement.
    pub location: StoryAppearanceLocation,
    /// Truthful resolved display name.
    pub display_name: String,
    /// Absolute Telegram publication timestamp.
    pub posted_at_ms: i64,
    /// Absolute expiration timestamp for active stories, when known.
    pub expires_at_ms: Option<i64>,
    /// Time a persistent profile appearance was observed removed, if any.
    pub removed_at_ms: Option<i64>,
    /// Durable profile reconciliation generation that observed this row.
    pub profile_scan_generation: Option<u64>,
    /// Zero-based order from the first profile page's `pinned_story_ids`.
    pub profile_pin_order: Option<u32>,
}

/// One provider-facing appearance joined to its single canonical story row.
///
/// The appearance carries only placement metadata. Content identity, policy,
/// locators, and the optional verified blob remain owned by [`StoryState`], so
/// projecting the same story into another chat-list view never duplicates
/// canonical metadata or bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryProjection {
    /// Canonical story metadata and optional verified-byte link.
    pub story: StoryState,
    /// Active or persistent monthly placement of that canonical story.
    pub appearance: StoryAppearanceRecord,
}

/// Privacy-safe account-level progress for `loadActiveStories(storyListMain)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoryListProgressRecord {
    /// Session pass generation.
    pub generation: u64,
    /// Successful bounded load calls committed across passes.
    pub pages_loaded: u64,
    /// Whether TDLib returned the documented 404 exhaustion signal this pass.
    pub complete: bool,
    /// Last durable checkpoint time.
    pub updated_at_ms: i64,
}

/// Minimum sync fact retained after a story becomes inaccessible or is purged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryTombstone {
    /// Canonical story identity.
    pub story: StoryKey,
    /// Observation time; no deletion/expiry cause is inferred.
    pub observed_at_ms: i64,
    /// Whether a persistent profile appearance had previously been observed.
    pub had_profile: bool,
}

/// Archive permission proven from exact owner/member evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryArchiveEligibility {
    /// Rights have not yet been proven.
    Unknown,
    /// Current regular user owns this story chat.
    Owner,
    /// Current user is creator or administrator with `can_edit_stories`.
    Manageable,
    /// The known chat/member shape does not grant archive access.
    Ineligible,
    /// The current account type cannot own a story archive (for example a bot).
    AccountUnsupported,
    /// Rights lookup was unavailable; absence was not guessed.
    RightsUnavailable,
}

impl StoryArchiveEligibility {
    /// Stable SQLite representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Owner => "owner",
            Self::Manageable => "manageable",
            Self::Ineligible => "ineligible",
            Self::AccountUnsupported => "account_unsupported",
            Self::RightsUnavailable => "rights_unavailable",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "owner" => Ok(Self::Owner),
            "manageable" => Ok(Self::Manageable),
            "ineligible" => Ok(Self::Ineligible),
            "account_unsupported" => Ok(Self::AccountUnsupported),
            "rights_unavailable" => Ok(Self::RightsUnavailable),
            other => Err(StateError::CorruptRow {
                table: "story_sync_progress",
                detail: format!("unknown archive_eligibility '{other}'"),
            }),
        }
    }

    /// Whether an archived-story request is authorized.
    pub fn permits_archive(self) -> bool {
        matches!(self, Self::Owner | Self::Manageable)
    }
}

/// Privacy-safe state of one chat's bounded story scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorySyncPhase {
    /// No request has started for this scan generation.
    Pending,
    /// Bounded discovery is actively advancing durable cursors.
    Syncing,
    /// Active/profile discovery and any permitted archive scan reached a boundary.
    Ready,
    /// The scan cannot run because required source metadata is unavailable.
    Unavailable,
    /// A request or protocol failure stopped this session's scan.
    Failed,
    /// The owner explicitly cancelled the scan.
    Cancelled,
}

impl StorySyncPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Syncing => "syncing",
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "pending" => Ok(Self::Pending),
            "syncing" => Ok(Self::Syncing),
            "ready" => Ok(Self::Ready),
            "unavailable" => Ok(Self::Unavailable),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StateError::CorruptRow {
                table: "story_sync_progress",
                detail: format!("unknown phase '{other}'"),
            }),
        }
    }
}

/// Durable cursors and privacy-safe counters for one chat's story discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorySyncProgressRecord {
    /// Current scheduler phase.
    pub phase: StorySyncPhase,
    /// Whether authoritative active membership was committed.
    pub active_complete: bool,
    /// Inclusive TDLib cursor for the next profile page.
    pub profile_cursor: Option<i32>,
    /// Reconciliation generation used to remove stale profile appearances safely.
    pub profile_scan_generation: u64,
    /// Whether the profile scan reached its current end.
    pub profile_complete: bool,
    /// Exact evidence controlling archive requests.
    pub archive_eligibility: StoryArchiveEligibility,
    /// Inclusive TDLib cursor for the next archived page.
    pub archive_cursor: Option<i32>,
    /// Whether archived backfill is complete or ineligible.
    pub archive_complete: bool,
    /// Number of metadata pages atomically committed.
    pub pages_committed: u64,
    /// Number of byte-free observations committed, including replays.
    pub stories_seen: u64,
    /// Privacy-safe failure class, without Telegram content.
    pub failure_category: Option<String>,
    /// Whether relaunch may re-queue the failed scan.
    pub retryable: bool,
    /// Number of failed request attempts observed for this scan.
    pub attempt_count: u32,
    /// Last durable progress update time.
    pub updated_at_ms: i64,
}

impl ReadTxn<'_> {
    /// Reads one canonical story by identity.
    pub fn story(&self, key: &StoryKey) -> Result<Option<StoryState>, StateError> {
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        let raw = self
            .conn()
            .query_row(
                "SELECT source_timestamp_ms, mime_type, exact_size, content_version,
                        availability, can_be_forwarded, content_state,
                        blob_hash_algo, blob_hash, last_verified_at_ms,
                        inaccessible_at_ms
                 FROM stories WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4",
                params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, bool>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<Vec<u8>>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                    ))
                },
            )
            .optional()?;
        raw.map(|raw| {
            Ok(StoryState {
                facts: StoryFacts {
                    key: *key,
                    source_timestamp_ms: raw.0,
                    mime_type: raw.1,
                    exact_size: raw.2.map(|v| size_from_column("stories", v)).transpose()?,
                    content_version: ContentVersion::new(raw.3).map_err(|error| {
                        StateError::CorruptRow {
                            table: "stories",
                            detail: format!("content_version does not parse: {error}"),
                        }
                    })?,
                    availability: AttachmentAvailability::parse(&raw.4)?,
                    can_be_forwarded: raw.5,
                    content_state: StoryContentState::parse(&raw.6)?,
                },
                blob_hash: hash_from_columns("stories", raw.7, raw.8)?,
                last_verified_at_ms: raw.9,
                inaccessible_at_ms: raw.10,
                locators: self.story_content_locators(key)?,
            })
        })
        .transpose()
    }

    /// Reads the active or persistent appearances of one story.
    pub fn story_appearances(
        &self,
        story: &StoryKey,
    ) -> Result<Vec<StoryAppearanceRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&story.poster.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT location, year, month, display_name, posted_at_ms, expires_at_ms,
                    removed_at_ms, profile_scan_generation, profile_pin_order
             FROM story_appearances WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4 ORDER BY location",
        )?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                story.poster.chat_id.0,
                story.story_id.0
            ],
            |row| {
                let tag: String = row.get(0)?;
                let year: Option<i64> = row.get(1)?;
                let month: Option<i64> = row.get(2)?;
                let location = match (tag.as_str(), year, month) {
                    ("active", None, None) => StoryAppearanceLocation::Active,
                    ("month", Some(year), Some(month)) => StoryAppearanceLocation::Month {
                        year: u16::try_from(year).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        month: u8::try_from(month).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    },
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };
                Ok(StoryAppearanceRecord {
                    story: *story,
                    location,
                    display_name: row.get(3)?,
                    posted_at_ms: row.get(4)?,
                    expires_at_ms: row.get(5)?,
                    removed_at_ms: row.get(6)?,
                    profile_scan_generation: row
                        .get::<_, Option<i64>>(7)?
                        .map(|value| {
                            u64::try_from(value).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    7,
                                    rusqlite::types::Type::Integer,
                                    Box::new(error),
                                )
                            })
                        })
                        .transpose()?,
                    profile_pin_order: row
                        .get::<_, Option<i64>>(8)?
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                })
            },
        )?;
        rows.collect::<Result<_, _>>().map_err(StateError::from)
    }

    /// Reads every active or persistent story appearance of one chat.
    ///
    /// Rows are ordered by canonical story id and location, independent of
    /// discovery/page order. Audit-retained removed profile appearances are
    /// included; callers use their `removed_at_ms` together with retention
    /// policy to expose already-observed material without initiating a new
    /// download.
    pub fn story_projections_of_chat(
        &self,
        chat: &ChatKey,
    ) -> Result<Vec<StoryProjection>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT DISTINCT story_id FROM story_appearances
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3
             ORDER BY story_id",
        )?;
        let story_ids = statement
            .query_map(params![account_id, namespace, chat.chat_id.0], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut projections = Vec::new();
        for story_id in story_ids {
            let key = StoryKey {
                poster: *chat,
                story_id: StoryId(story_id),
            };
            let story = self.story(&key)?.ok_or(StateError::CorruptRow {
                table: "story_appearances",
                detail: "appearance has no canonical story".to_owned(),
            })?;
            for appearance in self.story_appearances(&key)? {
                projections.push(StoryProjection {
                    story: story.clone(),
                    appearance,
                });
            }
        }
        Ok(projections)
    }

    /// Whether a chat has a truthful Stories-view appearance: at least one
    /// current active story, or one still-present profile-pinned story.
    pub fn has_stories_view_member(&self, chat: &ChatKey) -> Result<bool, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn()
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM story_appearances
                      WHERE account_id = ?1 AND namespace_version = ?2
                        AND poster_chat_id = ?3
                        AND (location = 'active'
                          OR (location = 'month' AND profile_pin_order IS NOT NULL
                              AND removed_at_ms IS NULL))
                 )",
                params![account_id, namespace, chat.chat_id.0],
                |row| row.get(0),
            )
            .map_err(StateError::from)
    }

    /// Reads every typed locator for one canonical story in stable role order.
    pub fn story_content_locators(
        &self,
        story: &StoryKey,
    ) -> Result<Vec<StoryContentLocatorRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&story.poster.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT role, file_type, is_primary, local_file_id, remote_file_id,
                    remote_unique_id, size, expected_size, content_version
             FROM story_content_locators
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4
             ORDER BY role",
        )?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                story.poster.chat_id.0,
                story.story_id.0
            ],
            |row| {
                let version: String = row.get(8)?;
                Ok(StoryContentLocatorRecord {
                    story: *story,
                    role: row.get(0)?,
                    file_type: StoryLocatorFileType::parse(&row.get::<_, String>(1)?).map_err(
                        |error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        },
                    )?,
                    is_primary: row.get(2)?,
                    local_file_id: row.get(3)?,
                    remote_file_id: row.get(4)?,
                    remote_unique_id: row.get(5)?,
                    size: row
                        .get::<_, Option<i64>>(6)?
                        .map(|value| size_from_column("story_content_locators", value))
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    expected_size: row
                        .get::<_, Option<i64>>(7)?
                        .map(|value| size_from_column("story_content_locators", value))
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    content_version: ContentVersion::new(version).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            8,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                })
            },
        )?;
        rows.collect::<Result<_, _>>().map_err(StateError::from)
    }

    /// Reads the minimal inaccessible tombstone for one story.
    pub fn story_tombstone(&self, key: &StoryKey) -> Result<Option<StoryTombstone>, StateError> {
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        self.conn()
            .query_row(
                "SELECT observed_at_ms, had_profile FROM story_tombstones
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4",
                params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
                |row| {
                    Ok(StoryTombstone {
                        story: *key,
                        observed_at_ms: row.get(0)?,
                        had_profile: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Reads one chat's resumable story discovery progress.
    pub fn story_sync_progress(
        &self,
        chat: &ChatKey,
    ) -> Result<Option<StorySyncProgressRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let raw = self
            .conn()
            .query_row(
                "SELECT phase, active_complete, profile_cursor,
                        profile_scan_generation, profile_complete,
                        archive_eligibility, archive_cursor, archive_complete,
                        pages_committed, stories_seen, failure_category,
                        retryable, attempt_count, updated_at_ms
                 FROM story_sync_progress
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
                params![account_id, namespace, chat.chat_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, Option<i32>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i32>>(6)?,
                        row.get::<_, bool>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, bool>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                    ))
                },
            )
            .optional()?;
        raw.map(|raw| {
            Ok(StorySyncProgressRecord {
                phase: StorySyncPhase::parse(&raw.0)?,
                active_complete: raw.1,
                profile_cursor: raw.2,
                profile_scan_generation: u64::try_from(raw.3).map_err(|_| {
                    StateError::CorruptRow {
                        table: "story_sync_progress",
                        detail: format!("negative profile_scan_generation {}", raw.3),
                    }
                })?,
                profile_complete: raw.4,
                archive_eligibility: StoryArchiveEligibility::parse(&raw.5)?,
                archive_cursor: raw.6,
                archive_complete: raw.7,
                pages_committed: u64::try_from(raw.8).map_err(|_| StateError::CorruptRow {
                    table: "story_sync_progress",
                    detail: format!("negative pages_committed {}", raw.8),
                })?,
                stories_seen: u64::try_from(raw.9).map_err(|_| StateError::CorruptRow {
                    table: "story_sync_progress",
                    detail: format!("negative stories_seen {}", raw.9),
                })?,
                failure_category: raw.10,
                retryable: raw.11,
                attempt_count: u32::try_from(raw.12).map_err(|_| StateError::CorruptRow {
                    table: "story_sync_progress",
                    detail: format!("invalid attempt_count {}", raw.12),
                })?,
                updated_at_ms: raw.13,
            })
        })
        .transpose()
    }

    /// Reads account-level `storyListMain` load progress.
    pub fn story_list_progress(
        &self,
        scope: gramdrive_model::identity::AccountScope,
    ) -> Result<Option<StoryListProgressRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        self.conn()
            .query_row(
                "SELECT generation, pages_loaded, complete, updated_at_ms
                 FROM story_list_progress
                 WHERE account_id = ?1 AND namespace_version = ?2",
                params![account_id, namespace],
                |row| {
                    let generation = row.get::<_, i64>(0)?;
                    let pages_loaded = row.get::<_, i64>(1)?;
                    Ok(StoryListProgressRecord {
                        generation: u64::try_from(generation).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        pages_loaded: u64::try_from(pages_loaded).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        complete: row.get(2)?,
                        updated_at_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Returns a bounded, stable worklist of chats whose story scans are not ready.
    pub fn story_sync_worklist(
        &self,
        scope: gramdrive_model::identity::AccountScope,
        limit: u32,
    ) -> Result<Vec<ChatKey>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT chat_id FROM story_sync_progress
             WHERE account_id = ?1 AND namespace_version = ?2
               AND phase IN ('pending', 'syncing')
             ORDER BY updated_at_ms, chat_id LIMIT ?3",
        )?;
        let rows =
            statement.query_map(params![account_id, namespace, i64::from(limit)], |row| {
                Ok(ChatKey {
                    scope,
                    chat_id: ChatId(row.get(0)?),
                })
            })?;
        rows.collect::<Result<_, _>>().map_err(StateError::from)
    }
}

impl WriteTxn<'_> {
    /// Starts a fresh session pass without discarding cumulative page count.
    pub fn start_story_list_pass(
        &self,
        scope: gramdrive_model::identity::AccountScope,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let changed = self.conn().execute(
            "INSERT INTO story_list_progress (
                 account_id, namespace_version, generation, pages_loaded,
                 complete, updated_at_ms)
             SELECT account_id, namespace_version, 1, 0, 0, ?3
             FROM accounts
             WHERE account_id = ?1 AND namespace_version = ?2
             ON CONFLICT (account_id, namespace_version) DO UPDATE SET
                 generation = story_list_progress.generation + 1,
                 complete = 0,
                 updated_at_ms = excluded.updated_at_ms",
            params![account_id, namespace, updated_at_ms],
        )?;
        if changed != 1 {
            return Err(StateError::RowNotFound { entity: "account" });
        }
        Ok(())
    }

    /// Commits one bounded `loadActiveStories` call or its exhaustion signal.
    pub fn advance_story_list_progress(
        &self,
        scope: gramdrive_model::identity::AccountScope,
        complete: bool,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let changed = self.conn().execute(
            "UPDATE story_list_progress
             SET pages_loaded = pages_loaded + CASE WHEN ?3 THEN 0 ELSE 1 END,
                 complete = ?3, updated_at_ms = ?4
             WHERE account_id = ?1 AND namespace_version = ?2",
            params![account_id, namespace, complete, updated_at_ms],
        )?;
        if changed != 1 {
            return Err(StateError::RowNotFound {
                entity: "story_list_progress",
            });
        }
        Ok(())
    }

    /// Applies authoritative chat-level protection before any story scan can
    /// run. Existing story content metadata and canonical byte links are
    /// destructively redacted, while byte-free appearances remain honest
    /// placeholders. Progress becomes explicitly unavailable.
    pub fn protect_chat_stories(
        &self,
        chat: &ChatKey,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn().execute(
            "UPDATE stories
             SET mime_type = NULL, exact_size = NULL,
                 content_version = 'story-protected/' || poster_chat_id || '/' || story_id,
                 availability = 'restricted', can_be_forwarded = 0,
                 content_state = 'protected', inaccessible_at_ms = NULL,
                 blob_hash_algo = NULL, blob_hash = NULL, last_verified_at_ms = NULL
             WHERE account_id = ?1 AND namespace_version = ?2 AND poster_chat_id = ?3",
            params![account_id, namespace, chat.chat_id.0],
        )?;
        self.conn().execute(
            "UPDATE story_sync_progress
             SET phase = 'unavailable', failure_category = 'chat-protected',
                 retryable = 0, attempt_count = 0, updated_at_ms = ?4
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            params![account_id, namespace, chat.chat_id.0, updated_at_ms],
        )?;
        Ok(())
    }

    /// Re-queues a formerly protected chat from a fresh authoritative active,
    /// profile, and rights boundary without discarding cumulative counters.
    pub fn restart_story_chat_scan(
        &self,
        chat: &ChatKey,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn().execute(
            "UPDATE story_sync_progress
             SET phase = 'pending', active_complete = 0, profile_cursor = NULL,
                 profile_scan_generation = profile_scan_generation + 1,
                 profile_complete = 0, archive_eligibility = 'unknown',
                 archive_cursor = NULL, archive_complete = 0,
                 failure_category = NULL, retryable = 0, attempt_count = 0,
                 updated_at_ms = ?4
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            params![account_id, namespace, chat.chat_id.0, updated_at_ms],
        )?;
        Ok(())
    }

    /// Starts a fresh active/profile/archive reconciliation for chats whose
    /// previous session reached ready. A new archive pass begins at TDLib's
    /// newest boundary so stories created and expired entirely during downtime
    /// are discoverable, and rights are proven again before that pass. Scans
    /// interrupted between page commits retain their exact durable cursors.
    /// A retryable failed scan is re-queued on relaunch without resetting its
    /// durable cursors, preventing an in-session hot loop while still making
    /// crash/relaunch recovery automatic.
    pub fn restart_ready_story_scans(
        &self,
        scope: gramdrive_model::identity::AccountScope,
        updated_at_ms: i64,
    ) -> Result<u64, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let ready = self.conn().execute(
            "UPDATE story_sync_progress
             SET phase = 'pending', active_complete = 0, profile_cursor = NULL,
                 profile_scan_generation = profile_scan_generation + 1,
                 profile_complete = 0, archive_eligibility = 'unknown',
                 archive_cursor = NULL, archive_complete = 0,
                 failure_category = NULL, retryable = 0, attempt_count = 0,
                 updated_at_ms = ?3
             WHERE account_id = ?1 AND namespace_version = ?2 AND phase = 'ready'",
            params![account_id, namespace, updated_at_ms],
        )?;
        let retryable = self.conn().execute(
            "UPDATE story_sync_progress
             SET phase = 'pending', failure_category = NULL, retryable = 0,
                 updated_at_ms = ?3
             WHERE account_id = ?1 AND namespace_version = ?2
               AND phase = 'failed' AND retryable = 1",
            params![account_id, namespace, updated_at_ms],
        )?;
        u64::try_from(ready.saturating_add(retryable)).map_err(|_| StateError::InvalidArgument {
            what: "story scan restart count exceeds u64",
        })
    }

    /// Inserts or refreshes canonical story facts without duplicating bytes.
    pub fn upsert_story(&self, facts: &StoryFacts) -> Result<(), StateError> {
        if facts.mime_type.as_deref() == Some("") {
            return Err(StateError::InvalidArgument {
                what: "story mime_type must not be empty",
            });
        }
        if facts.content_state == StoryContentState::Protected
            && (facts.can_be_forwarded
                || facts.mime_type.is_some()
                || facts.exact_size.is_some()
                || facts.availability != AttachmentAvailability::Restricted)
        {
            return Err(StateError::InvalidArgument {
                what: "protected story must contain only restricted placeholder metadata",
            });
        }
        if facts.content_state == StoryContentState::Available
            && (!facts.can_be_forwarded || facts.availability != AttachmentAvailability::Fetchable)
        {
            return Err(StateError::InvalidArgument {
                what: "available story must be save-permitted and fetchable",
            });
        }
        if facts.content_state == StoryContentState::Inaccessible
            && facts.availability != AttachmentAvailability::Unavailable
        {
            return Err(StateError::InvalidArgument {
                what: "inaccessible story must not advertise fetchability",
            });
        }
        if matches!(
            facts.content_state,
            StoryContentState::MetadataPending
                | StoryContentState::Unsupported
                | StoryContentState::LiveUnavailable
        ) && (facts.can_be_forwarded
            || facts.mime_type.is_some()
            || facts.exact_size.is_some()
            || facts.availability != AttachmentAvailability::Unavailable)
        {
            return Err(StateError::InvalidArgument {
                what: "unavailable story state must not carry fetchable metadata",
            });
        }
        if facts.content_state == StoryContentState::MetadataPending
            && self.read().story(&facts.key)?.is_some()
        {
            // A replayed storyInfo is strictly less authoritative than an
            // already normalized story. Never let it erase protection truth,
            // a verified byte link, or supported representation metadata.
            return Ok(());
        }
        let (account_id, namespace) = scope_columns(&facts.key.poster.scope);
        self.conn().execute(
            "INSERT INTO stories (
                 account_id, namespace_version, poster_chat_id, story_id,
                 source_timestamp_ms, mime_type, exact_size, content_version,
                 availability, can_be_forwarded, content_state, inaccessible_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)
             ON CONFLICT (account_id, namespace_version, poster_chat_id, story_id)
             DO UPDATE SET source_timestamp_ms = excluded.source_timestamp_ms,
                 mime_type = excluded.mime_type, exact_size = excluded.exact_size,
                 content_version = excluded.content_version,
                 availability = excluded.availability,
                 can_be_forwarded = excluded.can_be_forwarded,
                 content_state = excluded.content_state,
                 inaccessible_at_ms = NULL,
                 blob_hash_algo = CASE
                     WHEN excluded.can_be_forwarded
                      AND excluded.content_version = content_version THEN blob_hash_algo
                 END,
                 blob_hash = CASE
                     WHEN excluded.can_be_forwarded
                      AND excluded.content_version = content_version THEN blob_hash
                 END,
                 last_verified_at_ms = CASE
                     WHEN excluded.can_be_forwarded
                      AND excluded.content_version = content_version THEN last_verified_at_ms
                 END",
            params![
                account_id,
                namespace,
                facts.key.poster.chat_id.0,
                facts.key.story_id.0,
                facts.source_timestamp_ms,
                facts.mime_type,
                facts.exact_size.map(size_to_column).transpose()?,
                facts.content_version.as_str(),
                facts.availability.as_str(),
                facts.can_be_forwarded,
                facts.content_state.as_str(),
            ],
        )?;
        self.conn().execute(
            "DELETE FROM story_tombstones
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4",
            params![
                account_id,
                namespace,
                facts.key.poster.chat_id.0,
                facts.key.story_id.0
            ],
        )?;
        Ok(())
    }

    /// Atomically refreshes canonical story facts and every permitted locator.
    pub fn upsert_story_with_locators(
        &self,
        facts: &StoryFacts,
        locators: &[StoryContentLocatorRecord],
    ) -> Result<(), StateError> {
        let preserve_authoritative = facts.content_state == StoryContentState::MetadataPending
            && self.read().story(&facts.key)?.is_some();
        if facts.content_state != StoryContentState::Available && !locators.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "unavailable or protected story must not carry locators",
            });
        }
        if facts.content_state == StoryContentState::Available {
            let primary_count = locators.iter().filter(|locator| locator.is_primary).count();
            if locators.is_empty() || primary_count != 1 {
                return Err(StateError::InvalidArgument {
                    what: "available story requires exactly one primary locator",
                });
            }
        }
        let mut roles = BTreeSet::new();
        for locator in locators {
            if locator.story != facts.key
                || locator.role.is_empty()
                || (!roles.insert(locator.role.as_str()))
                || (locator.local_file_id.is_none() && locator.remote_file_id.is_none())
            {
                return Err(StateError::InvalidArgument {
                    what: "story locator identity or role is invalid",
                });
            }
            if locator.is_primary && locator.content_version != facts.content_version {
                return Err(StateError::InvalidArgument {
                    what: "primary story locator version must match canonical content version",
                });
            }
        }
        self.upsert_story(facts)?;
        if preserve_authoritative {
            return Ok(());
        }
        let (account_id, namespace) = scope_columns(&facts.key.poster.scope);
        self.conn().execute(
            "DELETE FROM story_content_locators
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4",
            params![
                account_id,
                namespace,
                facts.key.poster.chat_id.0,
                facts.key.story_id.0
            ],
        )?;
        for locator in locators {
            self.conn().execute(
                "INSERT INTO story_content_locators (
                     account_id, namespace_version, poster_chat_id, story_id,
                     role, file_type, is_primary, local_file_id, remote_file_id,
                     remote_unique_id, size, expected_size, content_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    account_id,
                    namespace,
                    facts.key.poster.chat_id.0,
                    facts.key.story_id.0,
                    locator.role,
                    locator.file_type.as_str(),
                    locator.is_primary,
                    locator.local_file_id,
                    locator.remote_file_id,
                    locator.remote_unique_id,
                    locator.size.map(size_to_column).transpose()?,
                    locator.expected_size.map(size_to_column).transpose()?,
                    locator.content_version.as_str(),
                ],
            )?;
        }
        Ok(())
    }

    /// Clears prior first-page pin order before one new generation is applied.
    pub fn clear_profile_pin_order(&self, chat: &ChatKey) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn().execute(
            "UPDATE story_appearances SET profile_pin_order = NULL
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND location = 'month'",
            params![account_id, namespace, chat.chat_id.0],
        )?;
        Ok(())
    }

    /// Active -> month is a transition: the active row is removed in the
    /// same transaction before the persistent appearance is written.
    pub fn set_story_appearance(
        &self,
        appearance: &StoryAppearanceRecord,
    ) -> Result<(), StateError> {
        if appearance.display_name.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "story display_name must not be empty",
            });
        }
        if self.read().story(&appearance.story)?.is_none() {
            return Err(StateError::RowNotFound { entity: "story" });
        }
        let (account_id, namespace) = scope_columns(&appearance.story.poster.scope);
        let (tag, year, month) = match appearance.location {
            StoryAppearanceLocation::Active => ("active", None, None),
            StoryAppearanceLocation::Month { year, month } => {
                ("month", Some(i64::from(year)), Some(i64::from(month)))
            }
        };
        if tag == "month" {
            self.conn().execute(
                "DELETE FROM story_appearances WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'active'",
                params![
                    account_id,
                    namespace,
                    appearance.story.poster.chat_id.0,
                    appearance.story.story_id.0
                ],
            )?;
        } else {
            let persistent: bool = self.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2 AND poster_chat_id = ?3
                   AND story_id = ?4 AND location = 'month')",
                params![
                    account_id,
                    namespace,
                    appearance.story.poster.chat_id.0,
                    appearance.story.story_id.0
                ],
                |row| row.get(0),
            )?;
            if persistent {
                return Ok(());
            }
        }
        self.conn().execute(
            "INSERT INTO story_appearances (
                 account_id, namespace_version, poster_chat_id, story_id, location,
                 year, month, display_name, posted_at_ms, expires_at_ms, removed_at_ms,
                 profile_scan_generation, profile_pin_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT (account_id, namespace_version, poster_chat_id, story_id, location)
             DO UPDATE SET year = excluded.year, month = excluded.month,
                 display_name = excluded.display_name, posted_at_ms = excluded.posted_at_ms,
                 expires_at_ms = excluded.expires_at_ms, removed_at_ms = excluded.removed_at_ms,
                 profile_scan_generation = excluded.profile_scan_generation,
                 profile_pin_order = CASE
                     WHEN excluded.profile_scan_generation IS NULL THEN profile_pin_order
                     ELSE excluded.profile_pin_order
                 END",
            params![
                account_id,
                namespace,
                appearance.story.poster.chat_id.0,
                appearance.story.story_id.0,
                tag,
                year,
                month,
                appearance.display_name,
                appearance.posted_at_ms,
                appearance.expires_at_ms,
                appearance.removed_at_ms,
                appearance
                    .profile_scan_generation
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| StateError::InvalidArgument {
                        what: "profile scan generation exceeds SQLite INTEGER",
                    })?,
                appearance.profile_pin_order.map(i64::from),
            ],
        )?;
        Ok(())
    }

    /// Atomically replaces one chat's authoritative active membership.
    /// Stories that never had a profile appearance disappear with their
    /// active row; persistent profile stories survive ordinary active expiry.
    pub fn replace_active_stories(
        &self,
        chat: &ChatKey,
        observed: &[(StoryFacts, StoryAppearanceRecord)],
    ) -> Result<(), StateError> {
        let mut ids = BTreeSet::new();
        for (facts, appearance) in observed {
            if facts.key.poster != *chat
                || appearance.story != facts.key
                || appearance.location != StoryAppearanceLocation::Active
            {
                return Err(StateError::InvalidArgument {
                    what: "active story snapshot identity/location mismatch",
                });
            }
            if !ids.insert(facts.key.story_id.0) {
                return Err(StateError::InvalidArgument {
                    what: "active story snapshot contains a duplicate id",
                });
            }
            self.upsert_story(facts)?;
            self.set_story_appearance(appearance)?;
        }

        let (account_id, namespace) = scope_columns(&chat.scope);
        let existing = {
            let mut statement = self.conn().prepare_cached(
                "SELECT story_id FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND location = 'active'",
            )?;
            let rows = statement
                .query_map(params![account_id, namespace, chat.chat_id.0], |row| {
                    row.get::<_, i64>(0)
                })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for story_id in existing {
            if ids.contains(&story_id) {
                continue;
            }
            self.conn().execute(
                "DELETE FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'active'",
                params![account_id, namespace, chat.chat_id.0, story_id],
            )?;
            self.purge_orphan_story(&StoryKey {
                poster: *chat,
                story_id: StoryId(story_id),
            })?;
        }
        Ok(())
    }

    /// Applies authoritative profile removal according to account retention.
    pub fn remove_profile_story(
        &self,
        key: &StoryKey,
        retention: RetentionMode,
        observed_at_ms: i64,
    ) -> Result<(), StateError> {
        let had_profile = self.has_profile_appearance(key)?;
        if !had_profile {
            self.purge_orphan_story(key)?;
            return Ok(());
        }
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        match retention {
            RetentionMode::Mirror => {
                self.write_tombstone(key, observed_at_ms, true)?;
                self.conn().execute(
                    "DELETE FROM story_appearances
                     WHERE account_id = ?1 AND namespace_version = ?2
                       AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'month'",
                    params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
                )?;
                self.purge_orphan_story(key)?;
            }
            RetentionMode::Audit => {
                self.conn().execute(
                    "UPDATE story_appearances SET removed_at_ms = ?5
                     WHERE account_id = ?1 AND namespace_version = ?2
                       AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'month'",
                    params![
                        account_id,
                        namespace,
                        key.poster.chat_id.0,
                        key.story_id.0,
                        observed_at_ms
                    ],
                )?;
            }
        }
        Ok(())
    }

    /// Completes one crash-resumable profile scan. Only completion can remove
    /// rows from an older generation; a crash mid-scan leaves prior truth intact.
    pub fn finish_profile_scan(
        &self,
        chat: &ChatKey,
        generation: u64,
        retention: RetentionMode,
        observed_at_ms: i64,
    ) -> Result<(), StateError> {
        let generation = i64::try_from(generation).map_err(|_| StateError::InvalidArgument {
            what: "profile scan generation exceeds SQLite INTEGER",
        })?;
        let (account_id, namespace) = scope_columns(&chat.scope);
        let stale = {
            let mut statement = self.conn().prepare_cached(
                "SELECT story_id FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND location = 'month'
                   AND removed_at_ms IS NULL
                   AND COALESCE(profile_scan_generation, -1) <> ?4",
            )?;
            let rows = statement.query_map(
                params![account_id, namespace, chat.chat_id.0, generation],
                |row| row.get::<_, i64>(0),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for story_id in stale {
            self.remove_profile_story(
                &StoryKey {
                    poster: *chat,
                    story_id: StoryId(story_id),
                },
                retention,
                observed_at_ms,
            )?;
        }
        Ok(())
    }

    /// Reduces reasonless `updateStoryDeleted` without fabricating an expiry
    /// or deletion cause. Ordinary active-only stories are purged in both
    /// modes; Audit retains only previously profile-observed material.
    pub fn mark_story_inaccessible(
        &self,
        key: &StoryKey,
        retention: RetentionMode,
        observed_at_ms: i64,
    ) -> Result<(), StateError> {
        let had_profile = self.has_profile_appearance(key)?;
        self.write_tombstone(key, observed_at_ms, had_profile)?;
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        self.conn().execute(
            "DELETE FROM story_appearances
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'active'",
            params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
        )?;
        if retention == RetentionMode::Audit && had_profile {
            self.conn().execute(
                "UPDATE stories SET content_state = 'inaccessible', inaccessible_at_ms = ?5,
                        availability = 'unavailable'
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4",
                params![
                    account_id,
                    namespace,
                    key.poster.chat_id.0,
                    key.story_id.0,
                    observed_at_ms
                ],
            )?;
            self.conn().execute(
                "UPDATE story_appearances SET removed_at_ms = COALESCE(removed_at_ms, ?5)
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'month'",
                params![
                    account_id,
                    namespace,
                    key.poster.chat_id.0,
                    key.story_id.0,
                    observed_at_ms
                ],
            )?;
        } else {
            self.conn().execute(
                "DELETE FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4",
                params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
            )?;
            self.conn().execute(
                "DELETE FROM stories
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4",
                params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
            )?;
        }
        Ok(())
    }

    /// Persists one resumable story-discovery checkpoint.
    pub fn put_story_sync_progress(
        &self,
        chat: &ChatKey,
        progress: &StorySyncProgressRecord,
    ) -> Result<(), StateError> {
        if matches!(
            progress.phase,
            StorySyncPhase::Unavailable | StorySyncPhase::Failed
        ) != progress.failure_category.is_some()
        {
            return Err(StateError::InvalidArgument {
                what: "story sync failure phase/category mismatch",
            });
        }
        if progress.retryable
            && !matches!(
                progress.phase,
                StorySyncPhase::Unavailable | StorySyncPhase::Failed
            )
        {
            return Err(StateError::InvalidArgument {
                what: "story sync retryable flag requires unavailable/failed phase",
            });
        }
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn().execute(
            "INSERT INTO story_sync_progress (
                 account_id, namespace_version, chat_id, phase, active_complete,
                 profile_cursor, profile_scan_generation, profile_complete,
                 archive_eligibility, archive_cursor, archive_complete,
                 pages_committed, stories_seen, failure_category, retryable,
                 attempt_count, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT (account_id, namespace_version, chat_id) DO UPDATE SET
                 phase = excluded.phase, active_complete = excluded.active_complete,
                 profile_cursor = excluded.profile_cursor,
                 profile_scan_generation = excluded.profile_scan_generation,
                 profile_complete = excluded.profile_complete,
                 archive_eligibility = excluded.archive_eligibility,
                 archive_cursor = excluded.archive_cursor,
                 archive_complete = excluded.archive_complete,
                 pages_committed = excluded.pages_committed,
                 stories_seen = excluded.stories_seen,
                 failure_category = excluded.failure_category,
                 retryable = excluded.retryable,
                 attempt_count = excluded.attempt_count,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                account_id,
                namespace,
                chat.chat_id.0,
                progress.phase.as_str(),
                progress.active_complete,
                progress.profile_cursor,
                i64::try_from(progress.profile_scan_generation).map_err(|_| {
                    StateError::InvalidArgument {
                        what: "profile scan generation exceeds SQLite INTEGER",
                    }
                })?,
                progress.profile_complete,
                progress.archive_eligibility.as_str(),
                progress.archive_cursor,
                progress.archive_complete,
                i64::try_from(progress.pages_committed).map_err(|_| {
                    StateError::InvalidArgument {
                        what: "story pages_committed exceeds SQLite INTEGER",
                    }
                })?,
                i64::try_from(progress.stories_seen).map_err(|_| {
                    StateError::InvalidArgument {
                        what: "story stories_seen exceeds SQLite INTEGER",
                    }
                })?,
                progress.failure_category,
                progress.retryable,
                i64::from(progress.attempt_count),
                progress.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    fn has_profile_appearance(&self, key: &StoryKey) -> Result<bool, StateError> {
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        self.conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM story_appearances
                 WHERE account_id = ?1 AND namespace_version = ?2
                   AND poster_chat_id = ?3 AND story_id = ?4 AND location = 'month')",
                params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
                |row| row.get(0),
            )
            .map_err(StateError::from)
    }

    fn purge_orphan_story(&self, key: &StoryKey) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        self.conn().execute(
            "DELETE FROM stories
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4
               AND NOT EXISTS (
                   SELECT 1 FROM story_appearances
                   WHERE account_id = ?1 AND namespace_version = ?2
                     AND poster_chat_id = ?3 AND story_id = ?4
               )",
            params![account_id, namespace, key.poster.chat_id.0, key.story_id.0],
        )?;
        Ok(())
    }

    fn write_tombstone(
        &self,
        key: &StoryKey,
        observed_at_ms: i64,
        had_profile: bool,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        self.conn().execute(
            "INSERT INTO story_tombstones (
                 account_id, namespace_version, poster_chat_id, story_id,
                 observed_at_ms, had_profile)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, namespace_version, poster_chat_id, story_id)
             DO UPDATE SET observed_at_ms = MAX(observed_at_ms, excluded.observed_at_ms),
                           had_profile = MAX(had_profile, excluded.had_profile)",
            params![
                account_id,
                namespace,
                key.poster.chat_id.0,
                key.story_id.0,
                observed_at_ms,
                had_profile
            ],
        )?;
        Ok(())
    }

    /// Links verified bytes to a fetchable, save-permitted canonical story.
    pub fn link_story_blob(
        &self,
        key: &StoryKey,
        hash: &ContentHash,
        verified_at_ms: i64,
    ) -> Result<(), StateError> {
        let story = self
            .read()
            .story(key)?
            .ok_or(StateError::RowNotFound { entity: "story" })?;
        if !story.facts.can_be_forwarded
            || story.facts.availability != AttachmentAvailability::Fetchable
            || !story.locators.iter().any(|locator| locator.is_primary)
        {
            return Err(StateError::InvalidArgument {
                what: "restricted story bytes cannot be linked",
            });
        }
        if self.read().blob(key.poster.scope.account, hash)?.is_none() {
            return Err(StateError::RowNotFound { entity: "blob" });
        }
        let (account_id, namespace) = scope_columns(&key.poster.scope);
        let (algorithm, bytes) = hash_columns(hash);
        self.conn().execute(
            "UPDATE stories SET blob_hash_algo = ?5, blob_hash = ?6, last_verified_at_ms = ?7
             WHERE account_id = ?1 AND namespace_version = ?2
               AND poster_chat_id = ?3 AND story_id = ?4",
            params![
                account_id,
                namespace,
                key.poster.chat_id.0,
                key.story_id.0,
                algorithm,
                bytes,
                verified_at_ms
            ],
        )?;
        Ok(())
    }
}
