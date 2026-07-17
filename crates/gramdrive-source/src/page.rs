//! Paged enumeration and the change feed (SYNC-003, SYNC-004, SYNC-022;
//! TASK-260715-1j4ij3).
//!
//! # Enumeration is a snapshot (SYNC-003)
//!
//! `children` returns [`ItemPage`]s anchored to one listing snapshot: the
//! parent's [`MetadataVersion`] at the first page. Every page of one
//! enumeration reports the same `snapshot`, and within it pages are
//! repeatable with no duplicate and no missing children — a source that
//! cannot keep serving a snapshot must reject the continuation token
//! ([`SourceError::CursorRejected`](crate::SourceError::CursorRejected))
//! rather than splice two states together; duplicates or gaps across pages
//! are contract failures the conformance suite hunts (NFR-002).
//!
//! [`PageToken`] is minted by the source and opaque to the core (DEC-003):
//! a TDLib source may encode an `(order, chat_id)` position, a remote
//! source its server token. Tokens are *not* durable — they live within one
//! enumeration, unlike change cursors, which is why they carry no format
//! version of their own.
//!
//! # Changes advance a durable cursor (SYNC-004, SYNC-022)
//!
//! `changes` returns [`ChangePage`]s: normalized [`ItemChange`] events in
//! source order plus the [`ChangeCursor`] to persist once — and only once —
//! the page is applied transactionally (SYNC-022). Deletions arrive as
//! explicit [`ItemChange::Removed`] events; cache eviction is a different
//! concept and never appears here (SYNC-025).

use std::num::NonZeroU32;

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::MetadataVersion;

use crate::item::SourceItem;

/// Upper bound on a page token's UTF-8 length, in bytes.
///
/// Positions are short; the cap keeps a malfunctioning source from turning
/// every enumeration round-trip into an unbounded allocation.
pub const MAX_PAGE_TOKEN_BYTES: usize = 1024;

/// Opaque continuation token for one enumeration, minted by the source.
///
/// Valid only against the source that minted it and only while that source
/// can keep serving the snapshot it belongs to. Not durable — persist
/// nothing but [`ChangeCursor`]s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageToken {
    text: String,
}

impl PageToken {
    /// Wraps a source-minted token, validating it is well-formed.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidPageToken> {
        let text = text.into();
        if text.is_empty() {
            return Err(InvalidPageToken::Empty);
        }
        if text.len() > MAX_PAGE_TOKEN_BYTES {
            return Err(InvalidPageToken::TooLong { len: text.len() });
        }
        if let Some(position) = text.bytes().position(|b| b.is_ascii_control()) {
            return Err(InvalidPageToken::ForbiddenCharacter { position });
        }
        Ok(Self { text })
    }

    /// The token text. Opaque: only the minting source interprets it.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// Why a string cannot be a [`PageToken`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidPageToken {
    /// The token is empty; "no token" is `Option::None`, not `""`.
    Empty,
    /// The token exceeds [`MAX_PAGE_TOKEN_BYTES`].
    TooLong {
        /// The rejected length in bytes.
        len: usize,
    },
    /// The token contains an ASCII control character (including DEL).
    ForbiddenCharacter {
        /// Byte offset of the offending character.
        position: usize,
    },
}

impl std::fmt::Display for InvalidPageToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("page token is empty"),
            Self::TooLong { len } => {
                write!(
                    f,
                    "page token is {len} bytes; limit is {MAX_PAGE_TOKEN_BYTES}"
                )
            }
            Self::ForbiddenCharacter { position } => {
                write!(f, "page token has a control character at byte {position}")
            }
        }
    }
}

impl std::error::Error for InvalidPageToken {}

/// One `children` request: where to continue and how much to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    /// Continue after this token; `None` starts the enumeration.
    pub continuation: Option<PageToken>,
    /// Maximum items the caller will accept in this page. Never zero by
    /// construction; a source may return fewer, never more.
    pub max_items: NonZeroU32,
}

impl PageRequest {
    /// The first page of an enumeration.
    pub fn first(max_items: NonZeroU32) -> Self {
        Self {
            continuation: None,
            max_items,
        }
    }
}

/// One page of children (SYNC-003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPage {
    /// The parent's metadata version this enumeration is a snapshot of.
    /// Identical across every page of one enumeration; a differing value
    /// between pages is a contract failure.
    pub snapshot: MetadataVersion,
    /// The page's items, in the source's stable enumeration order.
    pub items: Vec<SourceItem>,
    /// Continuation for the next page; `None` means the enumeration is
    /// complete.
    pub next: Option<PageToken>,
}

/// One normalized change event (SYNC-022, SYNC-025).
// Upserted dominates every real change feed, so boxing it to shrink the
// rare Removed variant would add a heap allocation to nearly every element
// — the wrong trade for a transient value that is applied page by page and
// never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemChange {
    /// The item appeared or its metadata/content changed; carries the
    /// item's current state.
    Upserted(SourceItem),
    /// The item was removed at the source. Identity only — there is no
    /// current state to carry, which is the point (SYNC-025 decides
    /// tombstone vs removal downstream).
    Removed(ItemId),
}

/// One page of the change feed (SYNC-004, SYNC-022).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePage {
    /// Change events in source order (SYNC-022).
    pub changes: Vec<ItemChange>,
    /// The durable position after applying this page. Persist it in the
    /// same transaction as the applied state — never before.
    pub next: ChangeCursor,
    /// Whether the source had more changes ready when it cut this page.
    /// `false` means the feed is drained as of `next`; polling cadence is
    /// the engine's decision either way.
    pub more_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_text() {
        let token = PageToken::new("after:chat-42").unwrap();
        assert_eq!(token.as_str(), "after:chat-42");
    }

    #[test]
    fn token_rejects_empty() {
        assert_eq!(PageToken::new("").unwrap_err(), InvalidPageToken::Empty);
    }

    #[test]
    fn token_rejects_oversized() {
        let long = "t".repeat(MAX_PAGE_TOKEN_BYTES + 1);
        assert_eq!(
            PageToken::new(long).unwrap_err(),
            InvalidPageToken::TooLong {
                len: MAX_PAGE_TOKEN_BYTES + 1
            }
        );
        assert!(PageToken::new("t".repeat(MAX_PAGE_TOKEN_BYTES)).is_ok());
    }

    #[test]
    fn token_rejects_control_characters() {
        assert_eq!(
            PageToken::new("a\tb").unwrap_err(),
            InvalidPageToken::ForbiddenCharacter { position: 1 }
        );
    }

    #[test]
    fn first_request_has_no_continuation() {
        let request = PageRequest::first(NonZeroU32::new(200).unwrap());
        assert_eq!(request.continuation, None);
        assert_eq!(request.max_items.get(), 200);
    }

    #[test]
    fn token_error_messages_name_the_violation() {
        assert_eq!(InvalidPageToken::Empty.to_string(), "page token is empty");
        assert_eq!(
            InvalidPageToken::TooLong { len: 2000 }.to_string(),
            "page token is 2000 bytes; limit is 1024"
        );
        assert_eq!(
            InvalidPageToken::ForbiddenCharacter { position: 1 }.to_string(),
            "page token has a control character at byte 1"
        );
    }
}
