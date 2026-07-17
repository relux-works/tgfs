//! Cross-platform naming policy (TASK-260715-1ffbkg; SYNC-012, SYNC-013,
//! PLAT-021, POL-1).
//!
//! Telegram titles are untrusted input. They arrive with path separators,
//! control characters, bidirectional overrides, Windows device names, 4 KB of
//! emoji, or nothing at all, and the tree builder passes them through
//! unchanged on purpose (`crate::tree`) — projecting a raw string onto a
//! filesystem is this module's job.
//!
//! [`sanitize`] is total: every input, however hostile, yields one
//! [`SafeName`] that every supported platform accepts. [`resolve_siblings`]
//! then makes a set of sibling names collision-free, deriving disambiguating
//! suffixes from stable identity rather than from discovery order (SYNC-012).
//!
//! # One name for the strictest target (SYNC-013)
//!
//! There is exactly one sanitized name per item, not one per platform. The
//! same archive is projected by the macOS File Provider, the Windows CfAPI
//! host, the Android `DocumentsProvider`, and the Linux FUSE adapter; a name
//! that differed per platform would make the same chat a different path
//! depending on where it is read, and `chat.json` links (SYNC-032) would stop
//! resolving across platforms.
//!
//! So the policy is the *union* of every platform's restrictions, and
//! [`Platform`] models each platform faithfully enough for the fixture corpus
//! to assert that against the platform rather than against this code
//! (PLAT-021). That takes two models, because a platform rejects names one at
//! a time and merges them in pairs:
//!
//! - [`Platform::check`] — which single names the platform accepts.
//! - [`Platform::fold`] — which *two* names the platform treats as one entry.
//!
//! The second is not a detail of the first. A corpus of one-name-per-row
//! expectations, however large, cannot catch a wrong case fold: every row
//! passes while two rows silently name one folder. Windows folds by NTFS's
//! uppercase table and Apple by Unicode case folding, and neither contains the
//! other — `ı`/`i` collide only on Windows, Kelvin `K`/`K` only on Apple — so
//! [`resolve_siblings`] folds through every platform rather than trusting one.
//!
//! # What the policy does, and why each rule exists
//!
//! In pipeline order — the order matters, see [`sanitize`]:
//!
//! 1. **Invisible characters are removed.** C0/C1 controls and DEL are
//!    rejected outright by Windows and are unreadable everywhere. Bidi
//!    overrides and isolates (U+202A–U+202E, U+2066–U+2069) and the LRM/RLM
//!    marks are a spoofing vector, not a display preference: an override can
//!    render `photo_gnp.exe` as `photo_exe.png`. This rule is GramDrive
//!    policy rather than filesystem truth — no platform forbids bidi controls
//!    — which is why [`Platform::check`] does not test for it and
//!    [`SafeName::parse`] does.
//!
//!    ZWJ (U+200D) and ZWNJ (U+200C) are deliberately **kept**: they carry
//!    meaning. Stripping ZWJ tears a family emoji into separate people, and
//!    stripping ZWNJ corrupts Persian and Indic text.
//! 2. **Visible forbidden characters become `_`.** `< > : " / \ | ? *` are
//!    the Windows set and a superset of everyone else's. Substituting rather
//!    than deleting keeps word boundaries legible; the ambiguity it creates
//!    (`a/b` and `a_b` both become `a_b`) is exactly what collision
//!    resolution below exists to settle.
//! 3. **NFC normalization.** After the removals, never before: deleting a
//!    control from between a base character and its combining mark can leave
//!    a sequence that composes, so normalizing first would emit a name that
//!    is not NFC. NFC over NFD because Windows and Linux preserve bytes,
//!    APFS is normalization-insensitive, and one canonical form is what makes
//!    the case-folded collision check meaningful.
//! 4. **Edges are trimmed.** Trailing dots and spaces are silently dropped by
//!    Windows, so `Chat.` and `Chat` are the same name there and only one of
//!    them survives a round trip. Trimming makes ours the surviving one. It
//!    also dissolves `.` and `..` into the empty string, which is why no
//!    crafted title can produce a path segment (see *Traversal*).
//! 5. **An empty result becomes [`FALLBACK_NAME`].** Chats can legitimately
//!    have no title, and a title of `"..."` legitimately sanitizes to
//!    nothing. Two such chats then collide, and collision resolution gives
//!    them identity-derived suffixes — the same mechanism, no special case.
//! 6. **Truncation to the component budget, at grapheme boundaries.** Cutting
//!    at a byte boundary produces invalid UTF-8; at a codepoint boundary it
//!    splits 👨‍👩‍👧‍👦 into a lone 👨 or leaves a dangling ZWJ, and halves a
//!    flag into a stray regional indicator. Grapheme clusters are the only
//!    boundary at which a cut is invisible to the reader.
//! 7. **Windows device names are escaped** with a trailing underscore on the
//!    stem: `CON` becomes `CON_`, `CON.txt` becomes `CON_.txt` — the stem
//!    before the *first* dot is what Windows reserves, so appending to the
//!    end would not help.
//!
//! # Traversal is impossible by construction (AC)
//!
//! A [`SafeName`] is one path *component*, and no crafted title can widen it
//! into more: step 2 replaces both separators before any structure is
//! derived, and step 4 leaves `.` and `..` empty, so they reach step 5 and
//! come out as [`FALLBACK_NAME`]. `../../etc/passwd` sanitizes to a single
//! ordinary component. The guarantee is a property test
//! (`tests/naming_properties.rs`), not a review promise.
//!
//! # Whole-path budgets belong to the adapters (PLAT-022)
//!
//! This module budgets one component (255 UTF-8 bytes and 255 UTF-16 units,
//! the strictest of the four). It cannot budget a whole path: the core does
//! not know where a sync root is mounted, and the default layout nests six
//! levels deep, so a `MAX_PATH` of 260 could not be met by component
//! truncation without mangling names on the three platforms that have no such
//! limit. Windows long-path support is the CfAPI host's declared capability
//! (PLAT-WIN-004), documented as a capability rather than hidden here
//! (PLAT-022).

// `is_nfc`, not the cheaper `is_nfc_quick`: the quick check answers "yes",
// "no", or "maybe", and "maybe" is its answer for anything starting with a
// combining mark. Treating maybe as no would reject a lone accent, which is
// unusual text but perfectly normalized text.
use unicode_normalization::{UnicodeNormalization, is_nfc};
use unicode_segmentation::UnicodeSegmentation;

use crate::identity::ItemId;

/// Name given to an item whose title sanitizes to nothing.
///
/// Not unique on purpose: several untitled chats all land here and are then
/// separated by [`resolve_siblings`] exactly like any other collision.
pub const FALLBACK_NAME: &str = "Unnamed";

/// Character substituted for a forbidden visible character.
const REPLACEMENT: char = '_';

/// Characters Windows forbids in a path component. A superset of every other
/// supported platform's set, which is only `/`.
const WINDOWS_FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windows device names, reserved whatever the extension: `CON.txt` names the
/// console, not a file.
///
/// `COM0`, `LPT0`, `CONIN$` and `CONOUT$` are included although the classic
/// list omits them: they are reserved by some Windows versions and APIs, and
/// over-escaping costs one underscore while under-escaping costs a file that
/// cannot be created.
const RESERVED_STEMS: [&str; 26] = [
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    "CONIN$", "CONOUT$",
];

/// Longest tail (including the dot) [`split_extension`] will treat as an
/// extension. Long enough for the real ones (`.markdown`, `.ndjson`), short
/// enough that a chat titled `Bob 3.0 discussion notes` keeps its whole title
/// as the stem.
const MAX_EXTENSION_BYTES: usize = 17;

/// Base32 alphabet of the collision suffix — RFC 4648 lowercase, the same
/// alphabet [`ItemId::text`] uses, so every GramDrive-minted token in a path
/// reads alike.
const ALPHABET: [u8; 32] = *b"abcdefghijklmnopqrstuvwxyz234567";

/// Suffix widths tried in order, in base32 characters of the identity digest.
///
/// 7 characters (35 bits) settles every realistic collision; 13 spends the
/// whole digest. [`SUFFIX_WIDTHS`] running out escalates to the full
/// [`ItemId`] text, which cannot collide — see [`resolve_siblings`].
const SUFFIX_WIDTHS: [usize; 2] = [7, 13];

/// Base32 characters in a 64-bit digest, rounded up: `ceil(64 / 5)`.
const DIGEST_WIDTH: usize = 13;

/// Whether a name will be projected as a directory or as a file.
///
/// The distinction is only about extensions, and it is a parameter rather
/// than a guess because guessing is what gets it wrong: a chat titled
/// `Bob 3.0` has no extension, and truncating `photo.jpg` without keeping
/// `.jpg` produces a file whose type no platform can resolve (SYNC-032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameKind {
    /// A directory: account roots, chat-list views, chats, years, `media`.
    /// The whole name is the stem; no tail is protected.
    Directory,
    /// A file: attachments and generated documents. A trailing extension
    /// survives truncation and stays last when a collision suffix is added.
    File,
}

/// Per-component length limits of one platform.
///
/// Two units because the platforms disagree about what they count: NTFS
/// counts UTF-16 code units, the POSIX filesystems count bytes. Neither
/// bounds the other — `é` is one UTF-16 unit and two UTF-8 bytes, `𝄞` is two
/// UTF-16 units and four UTF-8 bytes — so a name must satisfy both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentBudget {
    /// Maximum length in UTF-8 bytes.
    pub utf8_bytes: usize,
    /// Maximum length in UTF-16 code units.
    pub utf16_units: usize,
}

impl ComponentBudget {
    /// The budget every sanitized name is fitted to: the tightest limit of
    /// every [`Platform::ALL`] entry, in each unit independently.
    ///
    /// Derived rather than written down, so adding a stricter platform
    /// tightens [`sanitize`] instead of leaving a constant to drift.
    pub fn strictest() -> Self {
        Platform::ALL
            .iter()
            .map(|platform| platform.component_budget())
            .fold(
                Self {
                    utf8_bytes: usize::MAX,
                    utf16_units: usize::MAX,
                },
                |accumulated, budget| Self {
                    utf8_bytes: accumulated.utf8_bytes.min(budget.utf8_bytes),
                    utf16_units: accumulated.utf16_units.min(budget.utf16_units),
                },
            )
    }

    /// Whether `name` fits in both units.
    pub fn admits(&self, name: &str) -> bool {
        name.len() <= self.utf8_bytes && utf16_len(name) <= self.utf16_units
    }

    /// This budget with `cost` (a `(bytes, units)` pair) already spent —
    /// saturating, so an over-large reservation yields a zero budget rather
    /// than wrapping.
    fn less(self, cost: (usize, usize)) -> Self {
        Self {
            utf8_bytes: self.utf8_bytes.saturating_sub(cost.0),
            utf16_units: self.utf16_units.saturating_sub(cost.1),
        }
    }
}

/// A platform the archive is projected onto (POL-5 and its successors).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// macOS and iOS — APFS through `NSFileProviderReplicatedExtension`.
    Apple,
    /// Windows — NTFS through the Cloud Files API.
    Windows,
    /// Android — ext4/f2fs through a `DocumentsProvider`.
    Android,
    /// Linux — ext4 and friends through FUSE.
    Linux,
}

impl Platform {
    /// Every supported platform. The fixture corpus asserts each accepts the
    /// single sanitized output (PLAT-021), and [`ComponentBudget::strictest`]
    /// folds over this list.
    pub const ALL: [Self; 4] = [Self::Apple, Self::Windows, Self::Android, Self::Linux];

    /// This platform's per-component limits.
    ///
    /// `usize::MAX` means "this platform does not count in this unit", not
    /// "unlimited": the other unit still binds, and the strictest fold
    /// ignores it.
    pub fn component_budget(self) -> ComponentBudget {
        match self {
            // NTFS counts UTF-16 code units.
            Self::Windows => ComponentBudget {
                utf8_bytes: usize::MAX,
                utf16_units: 255,
            },
            // APFS, ext4 and f2fs count bytes.
            Self::Apple | Self::Android | Self::Linux => ComponentBudget {
                utf8_bytes: 255,
                utf16_units: usize::MAX,
            },
        }
    }

    /// Whether this platform resolves sibling names case-insensitively —
    /// equivalently, whether [`Platform::fold`] is anything but the identity.
    ///
    /// Drives nothing in [`sanitize`] — the collision check folds through
    /// every platform unconditionally, because a name set must be unambiguous
    /// on all of them to be projectable on all of them.
    pub fn case_insensitive(self) -> bool {
        matches!(self, Self::Apple | Self::Windows)
    }

    /// The key this platform compares two sibling names by.
    ///
    /// Two names with the same key are **one** directory entry here: creating
    /// both is not two chats, it is one chat silently shadowing the other.
    /// This is filesystem truth in the same sense as [`Platform::check`], and
    /// it is the thing [`resolve_siblings`] has to defeat.
    ///
    /// # Each platform folds differently, and neither fold contains the other
    ///
    /// - **Windows** folds through NTFS's `$UpCase` table — *uppercase*,
    ///   not lowercase and not Unicode case folding. So `ı` and `i` are one
    ///   name on NTFS (both uppercase to `I`) though no lowercasing fold puts
    ///   them together, and `ΟΔΟΣ`/`οδοσ`/`οδος` are one name (all three
    ///   uppercase to `ΟΔΟΣ`) though lowercasing keeps final `ς` apart from
    ///   medial `σ`.
    /// - **Apple** folds through full Unicode default case folding, which is
    ///   neither mapping: it sends `Σ`, `σ` and `ς` alike to `σ`, and both `ß`
    ///   and `ẞ` to `ss`. It also folds the compatibility singletons — Kelvin
    ///   `K` (U+212A) folds to `k`, which uppercasing does *not* touch.
    /// - **Android** and **Linux** compare bytes: ext4 and f2fs are
    ///   case-sensitive, so only an exact repeat is a collision and the fold is
    ///   the name itself.
    ///
    /// Windows catches `ı`/`i` and Apple does not; Apple catches `K`/`K` and
    /// Windows does not. Neither is "the strict one", which is why
    /// [`resolve_siblings`] folds through *every* platform rather than
    /// picking one to trust.
    ///
    /// # Fidelity of the Windows model
    ///
    /// `str::to_uppercase` applies Unicode's *full* uppercase mappings, where
    /// `$UpCase` is a simple 1:1 table: `ß` uppercases to `SS` here and is
    /// left alone by NTFS. The model therefore merges a pair (`ß`/`ss`) that
    /// real NTFS keeps apart — stricter than the platform, never looser. That
    /// direction costs one collision suffix on a name that did not need it;
    /// the other direction would ship a tree where one chat's folder
    /// overwrites another's.
    pub fn fold(self, name: &str) -> String {
        match self {
            Self::Windows => name.to_uppercase().nfc().collect(),
            Self::Apple => caseless::default_case_fold_str(name).nfc().collect(),
            Self::Android | Self::Linux => name.to_string(),
        }
    }

    /// Whether this platform would accept `name` as a path component.
    ///
    /// Filesystem truth, not GramDrive policy: bidi controls pass here
    /// because no platform rejects them, and control characters pass on the
    /// POSIX platforms because those genuinely allow them. The policy is
    /// [`SafeName::parse`], which is strictly stronger than every platform.
    pub fn check(self, name: &str) -> Result<(), NameViolation> {
        if name.is_empty() {
            return Err(NameViolation::Empty);
        }
        if name == "." || name == ".." {
            return Err(NameViolation::DotSegment);
        }
        for (position, character) in name.char_indices() {
            if self.forbids(character) {
                return Err(NameViolation::ForbiddenCharacter {
                    character,
                    position,
                });
            }
        }
        // Windows drops these silently, making `Chat.` and `Chat` the same
        // name; the other platforms keep them verbatim.
        if self == Self::Windows && name.ends_with(['.', ' ']) {
            return Err(NameViolation::TrailingDotOrSpace);
        }
        if self == Self::Windows
            && let Some(stem) = reserved_stem(name)
        {
            return Err(NameViolation::ReservedStem { stem });
        }
        let budget = self.component_budget();
        if name.len() > budget.utf8_bytes {
            return Err(NameViolation::TooLongUtf8 {
                bytes: name.len(),
                budget: budget.utf8_bytes,
            });
        }
        let units = utf16_len(name);
        if units > budget.utf16_units {
            return Err(NameViolation::TooLongUtf16 {
                units,
                budget: budget.utf16_units,
            });
        }
        Ok(())
    }

    fn forbids(self, character: char) -> bool {
        // NUL terminates a path on every supported platform.
        if character == '\0' {
            return true;
        }
        match self {
            Self::Windows => {
                matches!(character, '\u{1}'..='\u{1f}') || WINDOWS_FORBIDDEN.contains(&character)
            }
            Self::Apple | Self::Android | Self::Linux => character == '/',
        }
    }
}

/// Why a string is not usable as a path component.
///
/// Reported by [`Platform::check`] and [`SafeName::parse`]. Diagnostic, not
/// recoverable: [`sanitize`] is the way to obtain a usable name, and these
/// tell a test or a bug report which rule a string broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameViolation {
    /// The name is empty.
    Empty,
    /// The name is `.` or `..` — the filesystem's own entries, never an item.
    DotSegment,
    /// The name is not in Unicode NFC.
    NotNormalized,
    /// An invisible character (control, bidi override, zero-width space).
    InvisibleCharacter {
        /// The offending character.
        character: char,
        /// Its byte offset within the name.
        position: usize,
    },
    /// A character the platform forbids outright.
    ForbiddenCharacter {
        /// The offending character.
        character: char,
        /// Its byte offset within the name.
        position: usize,
    },
    /// The name ends with a dot or a space, which Windows drops silently.
    TrailingDotOrSpace,
    /// The stem before the first dot is a Windows device name.
    ReservedStem {
        /// The reserved stem, as listed in [`RESERVED_STEMS`].
        stem: &'static str,
    },
    /// Too long in UTF-8 bytes.
    TooLongUtf8 {
        /// Actual length.
        bytes: usize,
        /// The applicable limit.
        budget: usize,
    },
    /// Too long in UTF-16 code units.
    TooLongUtf16 {
        /// Actual length.
        units: usize,
        /// The applicable limit.
        budget: usize,
    },
}

impl std::fmt::Display for NameViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("name is empty"),
            Self::DotSegment => f.write_str("name is a '.' or '..' path segment"),
            Self::NotNormalized => f.write_str("name is not in Unicode NFC"),
            Self::InvisibleCharacter {
                character,
                position,
            } => {
                write!(
                    f,
                    "invisible character U+{:04X} at byte {position}",
                    u32::from(*character)
                )
            }
            Self::ForbiddenCharacter {
                character,
                position,
            } => {
                write!(
                    f,
                    "forbidden character U+{:04X} at byte {position}",
                    u32::from(*character)
                )
            }
            Self::TrailingDotOrSpace => f.write_str("name ends with a dot or a space"),
            Self::ReservedStem { stem } => write!(f, "stem '{stem}' is a reserved device name"),
            Self::TooLongUtf8 { bytes, budget } => {
                write!(f, "name is {bytes} UTF-8 bytes, budget is {budget}")
            }
            Self::TooLongUtf16 { units, budget } => {
                write!(f, "name is {units} UTF-16 units, budget is {budget}")
            }
        }
    }
}

impl std::error::Error for NameViolation {}

/// One path component every supported platform accepts.
///
/// Minted by [`sanitize`] (total, never fails) or validated by
/// [`SafeName::parse`]. Both guarantee the same invariants, so a `SafeName`
/// that exists is projectable on Apple, Windows, Android and Linux alike, is
/// NFC, holds no invisible characters, and is a single component — never a
/// path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SafeName(String);

impl SafeName {
    /// Validates an existing string against the full policy.
    ///
    /// The policy is the union of every [`Platform::check`] plus the rules no
    /// filesystem enforces but GramDrive does: NFC, and no invisible
    /// characters.
    pub fn parse(name: &str) -> Result<Self, NameViolation> {
        if !is_nfc(name) {
            return Err(NameViolation::NotNormalized);
        }
        for (position, character) in name.char_indices() {
            if is_invisible(character) {
                return Err(NameViolation::InvisibleCharacter {
                    character,
                    position,
                });
            }
        }
        for platform in Platform::ALL {
            platform.check(name)?;
        }
        Ok(Self(name.to_string()))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the name, yielding the owned string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for SafeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SafeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The POL-1 stable folder name of a chat (DEC-013).
///
/// `<Display Name> — @<username>`, or the title alone when the chat has no
/// public username. Raw — the caller sanitizes; [`resolve_siblings`] does it
/// as part of naming a sibling set.
///
/// The username is what makes the common case unambiguous without a suffix:
/// two different `Alex` chats are `Alex — @alex_one` and `Alex — @alex_two`,
/// and only genuinely indistinguishable chats fall through to an
/// identity-derived suffix.
///
/// Stable is the point: the name changes only when Telegram's own title or
/// username changes, never because the chat moved in the list (POL-1,
/// SYNC-011).
pub fn chat_folder_name(title: &str, username: Option<&str>) -> String {
    match username {
        Some(username) => format!("{title} — @{username}"),
        None => title.to_string(),
    }
}

/// Projects one untrusted title onto a single safe path component.
///
/// Total: hostile input has no failure mode here, only a boring output. The
/// pipeline and the reason for each step are in the module docs; the short
/// version is strip invisibles, substitute forbidden characters, normalize to
/// NFC, trim edges, fall back if empty, fit the budget at grapheme
/// boundaries, escape Windows device names.
///
/// Sanitizing is idempotent, and the output always satisfies
/// [`SafeName::parse`] — both are property tests.
///
/// Uniqueness is *not* a property of this function: two titles routinely
/// sanitize to one name. [`resolve_siblings`] is what makes a set of names
/// projectable.
pub fn sanitize(raw: &str, kind: NameKind) -> SafeName {
    let (stem, extension) = prepare(raw, kind);
    SafeName(compose(&stem, &extension, "", ComponentBudget::strictest()))
}

/// One sibling awaiting a name.
#[derive(Debug, Clone, Copy)]
pub struct SiblingName<'a> {
    /// Stable identity — the only thing a collision suffix is derived from
    /// (SYNC-012).
    pub id: &'a ItemId,
    /// Raw, untrusted title. For a chat, the [`chat_folder_name`] form.
    pub raw: &'a str,
    /// How the sibling is projected.
    pub kind: NameKind,
    /// Whether `raw` is a GramDrive-owned constant rather than a title from
    /// the source — `order.json` at a chat-list root (POL-1), and nothing a
    /// user can influence. Fixed names never take a collision suffix; a
    /// colliding title yields to them instead. See [`resolve_siblings`].
    pub fixed: bool,
}

/// Names a set of siblings so that no two collide (SYNC-012).
///
/// Returns one [`SafeName`] per input, positionally. Siblings that do not
/// collide keep their plain sanitized name — the common case pays nothing.
/// Colliding siblings each receive a suffix derived from their [`ItemId`]:
/// `Bob (k3m9xq2)`.
///
/// # Determinism, and why not discovery order (SYNC-012)
///
/// The output is a pure function of the input *set*. Feeding the same
/// siblings in a different order yields the same name for each — asserted by
/// property test. A counter (`Bob (2)`) would fail exactly this: whichever
/// chat happened to be enumerated first would keep the bare name, and the
/// tree would reshuffle itself on a re-sync, moving files under readers who
/// have paths open.
///
/// Every member of a collision set is suffixed, including the first. Leaving
/// one bare would privilege an arbitrary member and, worse, would rename the
/// survivor when the other chat is deleted. Both forms of churn are real; the
/// symmetric one at least never presents a `Bob` that is silently only one of
/// two Bobs. Names still change when the *set* changes — collision resolution
/// is set-relative by nature — but never when only the order changes.
///
/// # Fixed names are the one exception ([`SiblingName::fixed`], POL-1)
///
/// A sibling marked `fixed` keeps its name unconditionally, and titles that
/// collide with it are the ones that yield. This does not reintroduce the
/// churn the symmetry above avoids, because the two objections do not apply
/// to a constant: it privileges nothing arbitrary (`order.json` is GramDrive's
/// name, not one chat's claim on it), and it cannot be deleted, so no survivor
/// is ever renamed by its disappearance. Suffixing it instead would be the
/// real bug — `order.json` is the name POL-1 publishes at every list root, and
/// a chat titled `order.json` must not be able to push the ordering metadata to
/// `order.json (k3m9xq2)` or, worse, hand a provider two children with one
/// name.
///
/// Fixed names must be distinct from each other; a set that collides fixed
/// with fixed has no resolution and is returned with the duplicates intact
/// (the caller is projecting two constants into one directory, which no
/// GramDrive layout does).
///
/// # Suffix derivation
///
/// The suffix is base32 of a mixed 64-bit digest of the `ItemId` bytes, not a
/// prefix of the id itself: sibling ids share long prefixes (format version,
/// kind tag, account scope) and differ only deep in the payload, so a prefix
/// would be identical across the very items being distinguished. The digest
/// is FNV-1a with a splitmix64 finalizer — non-cryptographic on purpose. A
/// forced digest collision is not an attack here, it is just another
/// collision, and the escalation below absorbs it deterministically.
///
/// Escalation runs until the whole set is case-fold distinct: 7 base32
/// characters, then 13, then the full `ItemId` text. The last one cannot
/// collide — distinct ids give distinct texts, and the text is a
/// paren-delimited token at the end of the name — so the loop terminates with
/// a unique set. It also covers the crafted case where a chat is titled to
/// look exactly like another chat's suffixed name; because the check runs on
/// the *final* names, that title simply joins the collision set and gets a
/// suffix of its own.
///
/// Collisions are detected under [`fold_key`], which composes every
/// [`Platform::fold`]: Apple and Windows resolve names case-insensitively and
/// by *different* tables, so a set is projectable only if it is unambiguous
/// under both. Two chats titled `ΟΔΟΣ` and `οδοσ` — one Greek word, capitals
/// and lowercase — are one folder on both, and both therefore get a suffix.
///
/// # Precondition
///
/// Ids must be distinct. Two siblings sharing one identity are the same item
/// twice, which the tree builder cannot produce; they would receive the same
/// name, since no identity-derived suffix can separate identical identities.
pub fn resolve_siblings(siblings: &[SiblingName<'_>]) -> Vec<SafeName> {
    let budget = ComponentBudget::strictest();
    let prepared: Vec<(String, String)> = siblings
        .iter()
        .map(|sibling| prepare(sibling.raw, sibling.kind))
        .collect();

    // Escalation level per sibling: an index into SUFFIX_WIDTHS, then the
    // full-id fallback. Level 0 means "no suffix yet".
    let mut levels = vec![0usize; siblings.len()];
    let mut names: Vec<String> = prepared
        .iter()
        .map(|(stem, extension)| compose(stem, extension, "", budget))
        .collect();

    // Bounded by construction: every round strictly raises the level of at
    // least one colliding sibling, levels stop at the full-id suffix, and at
    // that level distinct ids give distinct names. The cap is a backstop for
    // the precondition being violated, not part of the algorithm.
    for _ in 0..siblings.len().saturating_add(SUFFIX_WIDTHS.len()).min(64) {
        let colliding: Vec<usize> = collisions(&names)
            .into_iter()
            .filter(|index| !siblings[*index].fixed)
            .collect();
        // Empty means either nothing collides or every remaining collision is
        // fixed-with-fixed, which no suffix can settle. Both are done.
        if colliding.is_empty() {
            break;
        }
        for index in colliding {
            levels[index] = levels[index].saturating_add(1);
            let suffix = suffix_for(siblings[index].id, levels[index]);
            let (stem, extension) = &prepared[index];
            names[index] = compose(stem, extension, &suffix, budget);
        }
    }

    names.into_iter().map(SafeName).collect()
}

/// Indices of names that are not unique under case folding.
fn collisions(names: &[String]) -> Vec<usize> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names {
        *counts.entry(fold_key(name)).or_insert(0) += 1;
    }
    names
        .iter()
        .enumerate()
        .filter(|(_, name)| counts.get(&fold_key(name)).copied().unwrap_or(0) > 1)
        .map(|(index, _)| index)
        .collect()
}

/// The comparison key a sibling set must be distinct under: every
/// [`Platform::fold`], composed.
///
/// Composed rather than chosen, because no single platform's fold is the
/// strict one — Windows merges `ı`/`i` that Apple keeps apart, Apple merges
/// Kelvin `K`/`K` that Windows keeps apart (see [`Platform::fold`]). A key
/// that trusted either alone would ship the other's collisions. Nor is any
/// stock mapping a substitute: `to_lowercase` misses every Windows-only pair,
/// `to_uppercase` misses every Apple-only one, and the round trip
/// `to_uppercase().to_lowercase()` still misses `ẞ`/`ß`, which APFS folds
/// together and neither mapping reaches.
///
/// Derived from [`Platform::ALL`] rather than written out, for the reason
/// [`ComponentBudget::strictest`] folds over the same list: a platform added
/// later tightens the key instead of leaving a constant to drift. The
/// case-sensitive platforms fold to the name itself, so they cost a clone and
/// change nothing.
///
/// # What is assumed, and what is checked
///
/// Composing gives distinctness under the *first* fold applied for free, and
/// says nothing about the later ones: with `ALL = [Apple, Windows, ..]` the key
/// is `Windows(Apple(x))`, so `Apple(a) == Apple(b)` forces equal keys whatever
/// the outer folds do — but that a set distinct under this key is also distinct
/// under `Platform::Windows.fold` is a fact about Unicode's tables, not a
/// consequence of the composition. It is not assumed here — the
/// property suite folds resolved names by [`Platform::fold`], platform by
/// platform, precisely so this key cannot grade its own homework.
fn fold_key(name: &str) -> String {
    Platform::ALL
        .iter()
        .fold(name.to_string(), |key, platform| platform.fold(&key))
}

/// The suffix for escalation `level` (1-based; 0 means no suffix).
///
/// Levels beyond [`SUFFIX_WIDTHS`] use the full [`ItemId`] text, which is the
/// terminating case: it is unique per id.
fn suffix_for(id: &ItemId, level: usize) -> String {
    match SUFFIX_WIDTHS.get(level.saturating_sub(1)) {
        Some(&width) => format!(" ({})", digest_text(identity_digest(id), width)),
        None => format!(" ({})", id.text()),
    }
}

/// FNV-1a over the identity bytes, finished with the splitmix64 mixer.
///
/// FNV-1a alone avalanches poorly in the high bits for short inputs, and the
/// suffix takes the *high* bits first, so the finalizer is what makes a short
/// suffix actually spread. Wrapping arithmetic is deliberate: these are hash
/// steps, and `overflow-checks` is on in every profile.
fn identity_digest(id: &ItemId) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    // splitmix64 finalizer.
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// The top `width` base32 characters of `digest`, most significant first.
fn digest_text(digest: u64, width: usize) -> String {
    (0..width.min(DIGEST_WIDTH))
        .map(|index| {
            let offset = 5 * index;
            // The final chunk of a 64-bit value spans only four bits; shift it
            // up and pad with a zero rather than reading past the top.
            let chunk = if offset + 5 <= 64 {
                (digest >> (64 - offset - 5)) & 0x1f
            } else {
                (digest << (offset + 5 - 64)) & 0x1f
            };
            char::from(ALPHABET[chunk as usize])
        })
        .collect()
}

/// Runs the input-facing half of the pipeline: strip, substitute, normalize,
/// trim, fall back, and split off an extension for files.
///
/// Stops before the budget, because [`resolve_siblings`] fits the same stem
/// to different budgets as it adds suffixes.
fn prepare(raw: &str, kind: NameKind) -> (String, String) {
    let filtered: String = raw
        .chars()
        .filter(|character| !is_invisible(*character))
        .map(|character| {
            if WINDOWS_FORBIDDEN.contains(&character) {
                REPLACEMENT
            } else {
                character
            }
        })
        .collect();
    // After filtering, never before: removing a character from between a base
    // and its combining mark can leave a sequence that composes.
    let normalized: String = filtered.nfc().collect();
    let trimmed = trim_edges(&normalized);
    if trimmed.is_empty() {
        return (FALLBACK_NAME.to_string(), String::new());
    }
    match kind {
        NameKind::Directory => (trimmed.to_string(), String::new()),
        NameKind::File => {
            let (stem, extension) = split_extension(trimmed);
            (stem.to_string(), extension.to_string())
        }
    }
}

/// Builds the final component: `stem + suffix + extension`, fitted to
/// `budget`, with a reserved stem escaped.
///
/// Only the stem is truncated. The extension is bounded by
/// [`MAX_EXTENSION_BYTES`] and the suffix by the `ItemId` text, and both are
/// load-bearing — a half-written suffix would not disambiguate, and a lost
/// extension is a file no platform can type.
///
/// One byte and one unit are held back for the escape underscore
/// unconditionally. That wastes a byte on the 255 that names never reach, and
/// buys the guarantee that escaping cannot push a fitted name over the budget
/// — no re-truncation loop, no case analysis about whether truncation just
/// created a `CON`.
fn compose(stem: &str, extension: &str, suffix: &str, budget: ComponentBudget) -> String {
    let reserved_cost = (1, 1);
    let stem_budget = budget
        .less(cost(extension))
        .less(cost(suffix))
        .less(reserved_cost);

    let cut = truncate(stem, stem_budget);
    // Truncation can expose a trailing dot or space that was interior before.
    let cut = trim_edges(cut);
    let cut = if cut.is_empty() {
        truncate(FALLBACK_NAME, stem_budget)
    } else {
        cut
    };
    // Re-normalize only if the cut fell inside a combining sequence, which
    // the grapheme walk avoids but its codepoint fallback does not. NFC of an
    // already-NFC prefix can only compose or reorder, never grow.
    let cut: String = if is_nfc(cut) {
        cut.to_string()
    } else {
        cut.nfc().collect()
    };

    let escaped = escape_reserved(&cut);
    format!("{escaped}{suffix}{extension}")
}

/// Escapes a Windows device name by appending `_` to the stem before the
/// first dot — where Windows looks — rather than to the end of the name.
fn escape_reserved(name: &str) -> String {
    match reserved_stem(name) {
        Some(_) => {
            let end = name.find('.').unwrap_or(name.len());
            format!("{}_{}", &name[..end], &name[end..])
        }
        None => name.to_string(),
    }
}

/// The reserved device name `name`'s stem matches, if any.
///
/// Windows reserves the stem before the *first* dot, case-insensitively and
/// after its own trailing-space trim: `con`, `CON.txt` and `CON ` all name
/// the console.
fn reserved_stem(name: &str) -> Option<&'static str> {
    let end = name.find('.').unwrap_or(name.len());
    let stem = name[..end].trim_end_matches([' ', '.']).to_uppercase();
    RESERVED_STEMS
        .into_iter()
        .find(|reserved| *reserved == stem)
}

/// Trims leading whitespace, and trailing whitespace and dots.
///
/// Trailing dots and spaces because Windows drops them silently; leading
/// whitespace because `  Chat` and `Chat` are the same chat to a reader and
/// sorting them apart helps nobody. Leading dots survive: `.config` is a
/// legitimate title, and hiding the folder on the POSIX platforms is a
/// display quirk, not a correctness problem. `.` and `..` do not survive —
/// they trim to nothing and become [`FALLBACK_NAME`].
fn trim_edges(name: &str) -> &str {
    name.trim_start_matches(char::is_whitespace)
        .trim_end_matches(|character: char| character.is_whitespace() || character == '.')
}

/// Splits a trailing extension (dot included) from a file name.
///
/// Conservative: the dot must not lead the name, and the tail must be short
/// and free of whitespace and further dots. `photo.jpg` splits; `Bob 3.0` and
/// `notes.tar.gz` keep more of themselves than a greedy rule would give them
/// (`.gz` is the extension, `notes.tar` the stem — which is what the last-dot
/// rule yields, correctly).
fn split_extension(name: &str) -> (&str, &str) {
    let Some(dot) = name.rfind('.') else {
        return (name, "");
    };
    if dot == 0 {
        return (name, "");
    }
    let extension = &name[dot..];
    let plausible = extension.len() > 1
        && extension.len() <= MAX_EXTENSION_BYTES
        && !extension[1..].contains(|character: char| character.is_whitespace());
    if plausible {
        (&name[..dot], extension)
    } else {
        (name, "")
    }
}

/// Truncates to `budget` at a grapheme-cluster boundary.
///
/// Grapheme clusters, not codepoints, so an emoji ZWJ sequence or a flag is
/// never cut in half. A single cluster can exceed any budget on its own — a
/// base character with hundreds of combining marks — and then the grapheme
/// walk yields nothing; the codepoint fallback cuts inside the cluster
/// instead, which is ugly but keeps some of the user's text and stays valid
/// UTF-8. `compose` re-normalizes after that path.
fn truncate(name: &str, budget: ComponentBudget) -> &str {
    if budget.admits(name) {
        return name;
    }
    let cut = boundary(name.grapheme_indices(true), budget);
    if cut > 0 {
        return &name[..cut];
    }
    let cut = boundary(
        name.char_indices()
            .map(|(at, character)| (at, &name[at..at + character.len_utf8()])),
        budget,
    );
    &name[..cut]
}

/// The byte offset at which `pieces` stops fitting in `budget`.
fn boundary<'a>(pieces: impl Iterator<Item = (usize, &'a str)>, budget: ComponentBudget) -> usize {
    let mut bytes = 0usize;
    let mut units = 0usize;
    let mut end = 0usize;
    for (at, piece) in pieces {
        let (piece_bytes, piece_units) = cost(piece);
        if bytes + piece_bytes > budget.utf8_bytes || units + piece_units > budget.utf16_units {
            break;
        }
        bytes += piece_bytes;
        units += piece_units;
        end = at + piece.len();
    }
    end
}

/// A string's `(UTF-8 bytes, UTF-16 units)` cost.
fn cost(text: &str) -> (usize, usize) {
    (text.len(), utf16_len(text))
}

/// Length in UTF-16 code units — what NTFS counts.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Whether a character is invisible and therefore removed.
///
/// Controls and DEL because Windows rejects them and nothing renders them;
/// bidi overrides, isolates and the directional marks because they let a name
/// render as something other than what it is; ZWSP and BOM because they are
/// invisible separators a title has no business carrying into a path.
///
/// ZWJ (U+200D) and ZWNJ (U+200C) sit inside these ranges and are excluded on
/// purpose — they are meaningful text, not decoration. See the module docs.
fn is_invisible(character: char) -> bool {
    matches!(character,
        '\u{0}'..='\u{1f}'        // C0 controls, including NUL
        | '\u{7f}'..='\u{9f}'     // DEL and the C1 controls
        | '\u{200b}'              // zero-width space
        | '\u{200e}' | '\u{200f}' // left-to-right / right-to-left mark
        | '\u{202a}'..='\u{202e}' // bidi embedding and override
        | '\u{2066}'..='\u{2069}' // bidi isolates
        | '\u{feff}'              // BOM / zero-width no-break space
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictest_budget_is_the_tightest_of_every_platform() {
        let budget = ComponentBudget::strictest();
        assert_eq!(budget.utf8_bytes, 255);
        assert_eq!(budget.utf16_units, 255);
    }

    #[test]
    fn digest_text_spends_the_whole_digest_and_stops() {
        // 13 base32 characters cover 65 bits: the last one carries the low
        // four bits padded with a zero, and asking for more yields no more.
        assert_eq!(digest_text(u64::MAX, DIGEST_WIDTH).len(), DIGEST_WIDTH);
        assert_eq!(digest_text(u64::MAX, DIGEST_WIDTH + 5).len(), DIGEST_WIDTH);
        assert_eq!(digest_text(0, DIGEST_WIDTH), "a".repeat(DIGEST_WIDTH));
        // Every character is in the alphabet, for a digest whose chunks walk
        // the whole range.
        assert!(
            digest_text(0x0123_4567_89ab_cdef, DIGEST_WIDTH)
                .bytes()
                .all(|byte| ALPHABET.contains(&byte))
        );
    }

    /// The assumption the whole collision check rests on: if *any* platform
    /// merges two names, [`fold_key`] merges them too. Composing the folds
    /// does not give this — it gives it for the first fold applied and leaves
    /// the rest to Unicode's tables — so it is pinned rather than argued.
    #[test]
    fn fold_key_merges_whatever_any_platform_merges() {
        let pairs = [
            ("ΟΔΟΣ", "οδοσ"),   // Greek caps vs lowercase
            ("οδος", "οδοσ"),   // final vs medial sigma
            ("ı", "i"),         // Windows-only: $UpCase sends both to I
            ("\u{212a}", "K"),  // Apple-only, and settled earlier by NFC
            ("\u{1e9e}", "ß"),  // Apple-only: both case-fold to `ss`
            ("\u{1e9e}", "ss"), // ditto
            ("ß", "ss"),
            ("\u{fb01}", "fi"), // ligature
            ("Bob", "BOB"),
        ];
        for (a, b) in pairs {
            for platform in Platform::ALL {
                if platform.fold(a) == platform.fold(b) {
                    assert_eq!(
                        fold_key(a),
                        fold_key(b),
                        "{platform:?} folds {a:?} and {b:?} into one entry, the key does not"
                    );
                }
            }
        }

        // Each mapping the key could have been instead, and the pair that
        // rules it out. Documented here because "just lowercase it" is the
        // obvious simplification, and it shipped a bug.
        assert_ne!("ΟΔΟΣ".to_lowercase(), "οδοσ".to_lowercase());
        assert_ne!("\u{212a}".to_uppercase(), "K".to_uppercase());
        assert_ne!(
            "\u{1e9e}".to_uppercase().to_lowercase(),
            "ß".to_uppercase().to_lowercase()
        );

        // And the key is not merely coarse: distinct names stay distinct.
        assert_ne!(fold_key("Alice"), fold_key("Bob"));
        assert_ne!(fold_key("Chat 1"), fold_key("Chat 2"));
    }

    #[test]
    fn reserved_stem_matches_windows_rules() {
        assert_eq!(reserved_stem("CON"), Some("CON"));
        assert_eq!(reserved_stem("con"), Some("CON"));
        assert_eq!(reserved_stem("CON.txt"), Some("CON"));
        assert_eq!(reserved_stem("CON "), Some("CON"));
        assert_eq!(reserved_stem("CONSOLE"), None);
        assert_eq!(reserved_stem("MYCON"), None);
        assert_eq!(reserved_stem("COM1"), Some("COM1"));
        assert_eq!(reserved_stem("COM10"), None);
    }

    #[test]
    fn escape_reserved_appends_to_the_stem_not_the_name() {
        // Appending to the end would leave "CON.txt_", whose stem before the
        // first dot is still CON — still reserved.
        assert_eq!(escape_reserved("CON.txt"), "CON_.txt");
        assert_eq!(escape_reserved("CON"), "CON_");
        assert_eq!(escape_reserved("Console"), "Console");
    }

    #[test]
    fn split_extension_is_conservative() {
        assert_eq!(split_extension("photo.jpg"), ("photo", ".jpg"));
        assert_eq!(split_extension("notes.tar.gz"), ("notes.tar", ".gz"));
        assert_eq!(split_extension("Bob 3.0 notes"), ("Bob 3.0 notes", ""));
        assert_eq!(split_extension(".hidden"), (".hidden", ""));
        assert_eq!(split_extension("plain"), ("plain", ""));
        // Too long to be an extension.
        assert_eq!(
            split_extension("archive.averyveryverylongsuffix"),
            ("archive.averyveryverylongsuffix", "")
        );
    }

    #[test]
    fn trim_edges_dissolves_dot_segments() {
        assert_eq!(trim_edges("."), "");
        assert_eq!(trim_edges(".."), "");
        assert_eq!(trim_edges("..."), "");
        assert_eq!(trim_edges("  Chat  "), "Chat");
        assert_eq!(trim_edges("Chat..."), "Chat");
        assert_eq!(trim_edges(".config"), ".config");
    }

    #[test]
    fn zero_width_joiner_and_non_joiner_survive() {
        // Stripping these would tear apart family emoji and corrupt Persian.
        assert!(!is_invisible('\u{200c}'));
        assert!(!is_invisible('\u{200d}'));
        assert!(is_invisible('\u{200b}'));
        assert!(is_invisible('\u{202e}'));
    }
}
