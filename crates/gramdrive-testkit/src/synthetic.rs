//! Synthetic large-account fixtures (TASK-260715-1ceq7h).
//!
//! [`generate`] deterministically expands a small [`SyntheticSpec`] into a
//! whole account of source facts — thousands of chats, a hundred thousand
//! messages, attachments, folder memberships, list order — shaped the way a
//! real Telegram account is shaped: a few enormous chats and a long tail of
//! tiny ones, months of history, sparse media. The state store's schema and
//! EXPLAIN evidence run against it, and later performance tasks reuse the
//! same generator so "fast enough" is always measured against the same
//! account.
//!
//! The output is plain data in the model vocabulary (chat ids, message ids,
//! [`ChatListKind`] memberships). It deliberately knows nothing about SQL
//! or any consumer: gramdrive-state maps it to rows, a benchmark maps it to
//! whatever it measures.
//!
//! # Determinism
//!
//! Same [`SyntheticSpec`], same account, bit for bit — the same discipline
//! as the rest of this crate: all variation flows from [`SplitMix64`]
//! seeded by `spec.seed`, no clock, no environment. The tests pin totals
//! and a structural digest so a distribution change is a deliberate,
//! visible edit here, not drift.
//!
//! # The synthetic calendar
//!
//! Timestamps spread over a synthetic history that starts at
//! [`SYNTHETIC_EPOCH_MS`] (2024-01-01 UTC) and runs in uniform 31-day
//! months. Real calendars buy nothing for synthetic data, but a fixed-width
//! month makes [`partition_of`] pure integer arithmetic, so every consumer
//! (rendering partitions, year directories) computes identical partitions
//! without a date library.

use gramdrive_model::identity::{
    AccountScope, AttachmentIndex, ChatId, ChatListKind, FolderId, MessageId,
};

use crate::fixture;
use crate::rng::SplitMix64;

/// Start of the synthetic history: 2024-01-01T00:00:00Z in Unix
/// milliseconds.
pub const SYNTHETIC_EPOCH_MS: i64 = 1_704_067_200_000;

/// One synthetic month: exactly 31 days.
pub const SYNTHETIC_MONTH_MS: i64 = 31 * 24 * 60 * 60 * 1000;

/// First calendar year of the synthetic history.
pub const SYNTHETIC_FIRST_YEAR: u16 = 2024;

/// How many custom Telegram folders every synthetic account defines.
const FOLDER_COUNT: u32 = 8;

/// Custom folder ids start here; Telegram reserves 0 and 1 for the
/// built-in lists.
const FIRST_FOLDER_ID: i32 = 2;

/// Size and seed of a synthetic account.
///
/// [`SyntheticSpec::large_account`] is the acceptance-criteria fixture;
/// [`SyntheticSpec::small`] keeps generator unit tests fast. Any other
/// combination is fair game — the generator only requires
/// `chat_count >= 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticSpec {
    /// Seed for every draw the generator makes.
    pub seed: u64,
    /// How many chats the account has.
    pub chat_count: u32,
    /// Total messages across every chat.
    pub message_count: u32,
}

impl SyntheticSpec {
    /// The large-account fixture the TASK-260715-1ceq7h acceptance criteria
    /// name: thousands of chats, 100k+ messages.
    pub fn large_account() -> Self {
        Self {
            seed: 0x6772_616d_6472_7601, // "gramdrv" + 01
            chat_count: 2_048,
            message_count: 110_000,
        }
    }

    /// A small account for fast generator tests.
    pub fn small() -> Self {
        Self {
            seed: 0x6772_616d_6472_7602,
            chat_count: 16,
            message_count: 500,
        }
    }
}

/// One deterministic synthetic account: the expansion of a
/// [`SyntheticSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticAccount {
    /// Identity scope of every id in the account.
    pub scope: AccountScope,
    /// Display name of the account root.
    pub display_name: String,
    /// The custom Telegram folders the account defines.
    pub folders: Vec<SyntheticFolder>,
    /// Every chat, in generation order.
    pub chats: Vec<SyntheticChat>,
}

impl SyntheticAccount {
    /// Total messages across all chats.
    pub fn message_total(&self) -> u64 {
        self.chats.iter().map(|c| c.messages.len() as u64).sum()
    }

    /// Total attachments across all messages.
    pub fn attachment_total(&self) -> u64 {
        self.chats
            .iter()
            .flat_map(|c| &c.messages)
            .map(|m| m.attachments.len() as u64)
            .sum()
    }
}

/// One custom Telegram folder definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticFolder {
    /// Telegram folder id.
    pub folder_id: FolderId,
    /// Folder title.
    pub title: String,
}

/// Telegram chat class of a synthetic chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticChatType {
    /// One-on-one chat.
    Private,
    /// Basic group.
    Group,
    /// Supergroup.
    Supergroup,
    /// Broadcast channel.
    Channel,
}

/// One synthetic chat with its list memberships and messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticChat {
    /// Telegram chat id, sign-shaped like the real id space.
    pub chat_id: ChatId,
    /// Chat class.
    pub chat_type: SyntheticChatType,
    /// Raw title, including non-ASCII shapes.
    pub title: String,
    /// Public username, when the chat has one.
    pub username: Option<String>,
    /// Telegram protected-content flag (POL-4).
    pub is_protected: bool,
    /// Which chat lists carry this chat, with exact order.
    pub list_entries: Vec<SyntheticListEntry>,
    /// Messages in ascending id and time order.
    pub messages: Vec<SyntheticMessage>,
}

/// Membership of a chat in one chat list (POL-1 order facts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticListEntry {
    /// The list: Main, Archive, or a custom folder.
    pub list: ChatListKind,
    /// Telegram order value — larger sorts first.
    pub sort_order: i64,
    /// Pinned in this list.
    pub pinned: bool,
}

/// One synthetic message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticMessage {
    /// Telegram message id, strictly increasing within the chat.
    pub message_id: MessageId,
    /// Sender identity snapshot.
    pub sender_id: i64,
    /// Send time in the synthetic calendar; strictly increasing within the
    /// chat.
    pub sent_at_ms: i64,
    /// Edit time, for the few messages that were edited.
    pub edited_at_ms: Option<i64>,
    /// Whether a deletion of this message was observed (POL-3 tombstone).
    pub deleted: bool,
    /// Attachments, indexed from zero.
    pub attachments: Vec<SyntheticAttachment>,
}

/// One synthetic attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticAttachment {
    /// Ordinal within the message.
    pub index: AttachmentIndex,
    /// Original file name, extension matching the MIME type.
    pub original_name: String,
    /// MIME type.
    pub mime_type: String,
    /// Logical size in bytes.
    pub size: u64,
    /// Logical content identity token.
    pub content_version: String,
}

/// The synthetic-calendar partition a timestamp falls into.
///
/// Pure integer arithmetic over the uniform 31-day months; every consumer
/// that partitions by (year, month) must use this so partitions agree.
/// Timestamps before [`SYNTHETIC_EPOCH_MS`] clamp to the first month —
/// the generator never produces them.
pub fn partition_of(sent_at_ms: i64) -> (u16, u8) {
    let offset = sent_at_ms.saturating_sub(SYNTHETIC_EPOCH_MS).max(0);
    let month_index = offset / SYNTHETIC_MONTH_MS;
    let year = SYNTHETIC_FIRST_YEAR + (month_index / 12) as u16;
    let month = (month_index % 12) as u8 + 1;
    (year, month)
}

/// Expands `spec` into its account. Same spec, same account, always.
pub fn generate(spec: &SyntheticSpec) -> SyntheticAccount {
    let mut rng = SplitMix64::new(spec.seed);
    let chat_count = spec.chat_count.max(1);

    let folders = (0..FOLDER_COUNT)
        .map(|n| SyntheticFolder {
            folder_id: FolderId(FIRST_FOLDER_ID + n as i32),
            title: format!("Folder {n}"),
        })
        .collect();

    let message_counts = zipf_allocation(chat_count, spec.message_count);
    let chats = (0..chat_count)
        .map(|i| chat(&mut rng, i, message_counts[i as usize]))
        .collect();

    SyntheticAccount {
        scope: fixture::scope(),
        display_name: "Synthetic Account".to_owned(),
        folders,
        chats,
    }
}

/// Splits `total` messages over `chat_count` chats with a Zipf-like skew:
/// chat 0 is enormous, the tail is mostly empty — the shape of a real
/// account. Quadratic decay rather than harmonic: harmonic weights leave
/// every chat a trickle, and a fixture with no empty chats never exercises
/// the empty-chat paths. Integer arithmetic only, exact total.
fn zipf_allocation(chat_count: u32, total: u32) -> Vec<u32> {
    let weights: Vec<u64> = (0..u64::from(chat_count))
        .map(|i| 4_000_000 / ((i + 1) * (i + 4)))
        .collect();
    let weight_sum: u64 = weights.iter().sum();
    let mut counts: Vec<u32> = weights
        .iter()
        .map(|w| (u64::from(total) * w / weight_sum) as u32)
        .collect();
    let assigned: u32 = counts.iter().sum();
    // Flooring loses a remainder; the head chat absorbs it.
    counts[0] += total - assigned;
    counts
}

fn chat(rng: &mut SplitMix64, index: u32, message_count: u32) -> SyntheticChat {
    let chat_type = match rng.next_u64() % 20 {
        0..=7 => SyntheticChatType::Private,
        8..=11 => SyntheticChatType::Group,
        12..=16 => SyntheticChatType::Supergroup,
        _ => SyntheticChatType::Channel,
    };
    // Sign-shaped like Telegram's id space: users positive, groups
    // negative, channel-style ids far negative.
    let chat_id = match chat_type {
        SyntheticChatType::Private => ChatId(10_000 + i64::from(index)),
        SyntheticChatType::Group => ChatId(-(100_000 + i64::from(index))),
        SyntheticChatType::Supergroup | SyntheticChatType::Channel => {
            ChatId(-1_000_000_000_000 - i64::from(index))
        }
    };
    // A few non-ASCII shapes so name handling downstream sees them early.
    let title = match index % 9 {
        6 => format!("Чат №{index}"),
        7 => format!("隊伍 {index}"),
        8 => format!("Team 🚀 {index}"),
        _ => format!("Chat {index:04}"),
    };
    let username = (rng.next_u64() % 100 < 30).then(|| format!("chat_{index}"));
    let is_protected = rng.next_u64() % 100 < 3;

    let mut list_entries = Vec::with_capacity(2);
    let base_list = if rng.next_u64() % 100 < 10 {
        ChatListKind::Archive
    } else {
        ChatListKind::Main
    };
    list_entries.push(SyntheticListEntry {
        list: base_list,
        sort_order: sort_order(rng, index),
        pinned: rng.next_u64() % 100 < 2,
    });
    if rng.next_u64() % 100 < 25 {
        let folder = FolderId(FIRST_FOLDER_ID + (rng.next_u64() % u64::from(FOLDER_COUNT)) as i32);
        list_entries.push(SyntheticListEntry {
            list: ChatListKind::Folder(folder),
            sort_order: sort_order(rng, index),
            pinned: false,
        });
    }

    let messages = messages(rng, chat_type, chat_id, message_count);

    SyntheticChat {
        chat_id,
        chat_type,
        title,
        username,
        is_protected,
        list_entries,
        messages,
    }
}

/// A deterministic Telegram-style order value: index-descending with
/// per-draw jitter, so generation order and list order do not coincide.
fn sort_order(rng: &mut SplitMix64, index: u32) -> i64 {
    i64::from(u32::MAX - index) * 1_000 + (rng.next_u64() % 997) as i64
}

fn messages(
    rng: &mut SplitMix64,
    chat_type: SyntheticChatType,
    chat_id: ChatId,
    count: u32,
) -> Vec<SyntheticMessage> {
    if count == 0 {
        return Vec::new();
    }
    // Each chat's history occupies a contiguous window of the synthetic
    // calendar: a start month in the first two years and a span of up to
    // two years — so month partitions overlap across chats without
    // coinciding.
    let start_month = (rng.next_u64() % 24) as i64;
    let span_months = 1 + (rng.next_u64() % 24) as i64;
    let span_ms = span_months * SYNTHETIC_MONTH_MS;
    let window_start = SYNTHETIC_EPOCH_MS + start_month * SYNTHETIC_MONTH_MS;

    let mut result = Vec::with_capacity(count as usize);
    let mut message_id: i64 = 2;
    for j in 0..i64::from(count) {
        // Evenly spaced through the window; strictly increasing because
        // span_ms >> count for every reachable spec.
        let sent_at_ms = window_start + j * span_ms / i64::from(count);
        let edited_at_ms = (rng.next_u64() % 100 < 5)
            .then(|| sent_at_ms + 3_600_000 + (rng.next_u64() % 86_400_000) as i64);
        let deleted = rng.next_u64() % 100 < 2;
        let sender_id = match chat_type {
            SyntheticChatType::Private => {
                if rng.next_u64().is_multiple_of(2) {
                    fixture::FIXTURE_ACCOUNT_ID
                } else {
                    chat_id.0
                }
            }
            SyntheticChatType::Channel => chat_id.0,
            SyntheticChatType::Group | SyntheticChatType::Supergroup => {
                100_000 + (rng.next_u64() % 32) as i64
            }
        };

        result.push(SyntheticMessage {
            message_id: MessageId(message_id),
            sender_id,
            sent_at_ms,
            edited_at_ms,
            deleted,
            attachments: attachments(rng, chat_id, message_id),
        });
        // Telegram ids climb with gaps.
        message_id += 1 + (rng.next_u64() % 3) as i64;
    }
    result
}

const MEDIA_SHAPES: &[(&str, &str, &str)] = &[
    ("image/jpeg", "IMG", "jpg"),
    ("image/png", "Screenshot", "png"),
    ("video/mp4", "VID", "mp4"),
    ("audio/ogg", "Voice", "ogg"),
    ("application/pdf", "Document", "pdf"),
];

fn attachments(rng: &mut SplitMix64, chat_id: ChatId, message_id: i64) -> Vec<SyntheticAttachment> {
    let draw = rng.next_u64() % 100;
    // ~20% of messages carry media; a sliver of those is an album of two.
    let count = match draw {
        0..=2 => 2,
        3..=19 => 1,
        _ => return Vec::new(),
    };
    (0..count)
        .map(|index| {
            let (mime, stem, ext) =
                MEDIA_SHAPES[(rng.next_u64() % MEDIA_SHAPES.len() as u64) as usize];
            // Power-law-ish sizes: 1 KiB .. 32 MiB.
            let size = (1u64 << (10 + rng.next_u64() % 15)) + rng.next_u64() % 1_000;
            SyntheticAttachment {
                index: AttachmentIndex(index),
                original_name: format!("{stem}_{message_id}_{index}.{ext}"),
                mime_type: mime.to_owned(),
                size,
                content_version: format!("cv-{}-{message_id}-{index}", chat_id.0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let spec = SyntheticSpec::small();
        assert_eq!(generate(&spec), generate(&spec));

        let other = SyntheticSpec {
            seed: spec.seed + 1,
            ..spec
        };
        assert_ne!(generate(&spec), generate(&other), "the seed matters");
    }

    #[test]
    fn totals_match_the_spec_exactly() {
        for spec in [SyntheticSpec::small(), SyntheticSpec::large_account()] {
            let account = generate(&spec);
            assert_eq!(account.chats.len(), spec.chat_count as usize);
            assert_eq!(account.message_total(), u64::from(spec.message_count));
        }
    }

    #[test]
    fn large_account_meets_the_acceptance_bar() {
        let spec = SyntheticSpec::large_account();
        assert!(spec.chat_count >= 2_000, "thousands of chats");
        assert!(spec.message_count >= 100_000, "100k+ messages");
        let account = generate(&spec);
        assert!(
            account.attachment_total() > 10_000,
            "attachments in the tens of thousands, got {}",
            account.attachment_total()
        );
    }

    #[test]
    fn distribution_is_skewed_with_a_long_tail() {
        let account = generate(&SyntheticSpec::large_account());
        let head = account.chats[0].messages.len();
        let empty = account
            .chats
            .iter()
            .filter(|c| c.messages.is_empty())
            .count();
        assert!(head > 10_000, "the head chat is enormous, got {head}");
        assert!(empty > 100, "the tail is mostly quiet, got {empty} empty");
    }

    #[test]
    fn chat_ids_are_unique_and_type_shaped() {
        let account = generate(&SyntheticSpec::large_account());
        let mut ids: Vec<i64> = account.chats.iter().map(|c| c.chat_id.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), account.chats.len(), "chat ids must not collide");

        for chat in &account.chats {
            match chat.chat_type {
                SyntheticChatType::Private => assert!(chat.chat_id.0 > 0),
                SyntheticChatType::Group
                | SyntheticChatType::Supergroup
                | SyntheticChatType::Channel => assert!(chat.chat_id.0 < 0),
            }
        }
    }

    #[test]
    fn messages_are_strictly_ordered_within_a_chat() {
        let account = generate(&SyntheticSpec::small());
        for chat in &account.chats {
            for pair in chat.messages.windows(2) {
                assert!(pair[0].message_id.0 < pair[1].message_id.0, "ids climb");
                assert!(pair[0].sent_at_ms < pair[1].sent_at_ms, "time climbs");
            }
            for message in &chat.messages {
                if let Some(edited) = message.edited_at_ms {
                    assert!(edited > message.sent_at_ms, "edits follow sends");
                }
                for (i, attachment) in message.attachments.iter().enumerate() {
                    assert_eq!(attachment.index.0 as usize, i, "ordinals from zero");
                    assert!(attachment.size > 0);
                }
            }
        }
    }

    #[test]
    fn every_chat_has_a_primary_list_and_valid_folders() {
        let account = generate(&SyntheticSpec::large_account());
        let folder_ids: Vec<i32> = account.folders.iter().map(|f| f.folder_id.0).collect();
        for chat in &account.chats {
            assert!(!chat.list_entries.is_empty());
            assert!(matches!(
                chat.list_entries[0].list,
                ChatListKind::Main | ChatListKind::Archive
            ));
            for entry in &chat.list_entries[1..] {
                match entry.list {
                    ChatListKind::Folder(id) => {
                        assert!(folder_ids.contains(&id.0), "folder {} undeclared", id.0);
                    }
                    other => panic!("secondary membership must be a folder, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn partitions_follow_the_synthetic_calendar() {
        assert_eq!(partition_of(SYNTHETIC_EPOCH_MS), (2024, 1));
        assert_eq!(
            partition_of(SYNTHETIC_EPOCH_MS + SYNTHETIC_MONTH_MS - 1),
            (2024, 1)
        );
        assert_eq!(
            partition_of(SYNTHETIC_EPOCH_MS + SYNTHETIC_MONTH_MS),
            (2024, 2)
        );
        assert_eq!(
            partition_of(SYNTHETIC_EPOCH_MS + 12 * SYNTHETIC_MONTH_MS),
            (2025, 1)
        );

        let account = generate(&SyntheticSpec::small());
        for message in account.chats.iter().flat_map(|c| &c.messages) {
            let (year, month) = partition_of(message.sent_at_ms);
            assert!((2024..=2028).contains(&year));
            assert!((1..=12).contains(&month));
        }
    }

    /// Pins the exact expansion of the small spec via a structural digest.
    /// A change to any draw order or distribution constant re-cuts every
    /// downstream fixture; this failing is the alarm, and updating the
    /// digest is the deliberate acknowledgment.
    #[test]
    fn small_spec_expansion_is_pinned() {
        let account = generate(&SyntheticSpec::small());
        let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |value: i64| {
            digest ^= value as u64;
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for chat in &account.chats {
            fold(chat.chat_id.0);
            fold(chat.messages.len() as i64);
            for message in &chat.messages {
                fold(message.message_id.0);
                fold(message.sent_at_ms);
                fold(message.attachments.len() as i64);
            }
        }
        assert_eq!(digest, 0xeeac_7676_659a_6943, "expansion drifted");
    }
}
