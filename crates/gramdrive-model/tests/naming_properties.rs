//! Property suite for the naming policy (TASK-260715-1ffbkg; SYNC-012,
//! SYNC-013).
//!
//! The fixture corpus pins the cases someone thought of. This proves the
//! invariants over sampled input that nobody thought of, which is where an
//! untrusted-input policy actually earns its keep:
//!
//! - [`sanitize`] is total and its output always satisfies the full policy;
//! - no input can produce a path separator or a `.`/`..` segment (the
//!   traversal acceptance criterion, as a property rather than a case list);
//! - sanitizing is idempotent;
//! - a resolved sibling set is always collision-free and always order-
//!   independent (SYNC-012).
//!
//! The generator is weighted towards the characters that break filesystems —
//! separators, dots, controls, bidi overrides, combining marks, astral
//! planes, regional indicators — because uniformly random `char`s almost
//! never produce a `CON` or a trailing dot.

use std::collections::{HashMap, HashSet};

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ChatId, ChatKey, ItemId, ItemKey,
    NamespaceVersion,
};
use gramdrive_model::naming::{
    ComponentBudget, NameKind, Platform, SafeName, SiblingName, sanitize,
};
use proptest::prelude::*;

/// Characters chosen to hit the rules: both separators, every Windows
/// forbidden character, NUL and other controls, bidi overrides and isolates,
/// zero-width joiners, combining marks, the letters that spell device names,
/// multi-byte and multi-cluster text, and the case-variant letters the
/// platforms fold differently (see [`CASE_VARIANTS`]) so that a device name
/// or a truncation can compound with a fold.
const NASTY: [char; 38] = [
    '/', '\\', '.', ' ', ':', '<', '>', '"', '|', '?', '*', '\0', '\u{7}', '\u{7f}', '\u{9f}',
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{202e}', '\u{2066}', '\u{feff}', '\u{301}', '\u{323}',
    'C', 'O', 'N', 'M', 'L', 'P', 'T', '1', 'é', '😀', '家', 'Σ', 'ς', 'ı', 'ß',
];

fn arb_char() -> impl Strategy<Value = char> {
    prop_oneof![
        6 => prop::sample::select(NASTY.as_slice()),
        1 => any::<char>(),
    ]
}

/// Long enough to overrun the 255-byte budget when the sample runs to
/// multi-byte characters, which is the only way to exercise truncation.
fn arb_raw() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_char(), 0..200).prop_map(|chars| chars.into_iter().collect())
}

/// Characters that fold together with another member of this set, on at least
/// one platform: the Greek sigmas (medial, final, capital), the Turkish and
/// Latin i family, the compatibility singletons Kelvin and Angstrom, and both
/// sharp s forms. Every pair here is a case the platforms disagree about —
/// `ı`/`i` collide only on Windows, `K`/`K` only on Apple.
const CASE_VARIANTS: [char; 18] = [
    'Σ', 'σ', 'ς', 'I', 'i', 'ı', 'İ', 'K', '\u{212a}', 'ß', '\u{1e9e}', 'S', 's', 'A', 'a',
    '\u{212b}', 'Å', 'å',
];

fn arb_kind() -> impl Strategy<Value = NameKind> {
    prop_oneof![Just(NameKind::Directory), Just(NameKind::File)]
}

fn chat_id(id: i64) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
        scope: AccountScope {
            account: AccountKey {
                account_id: AccountId(42),
            },
            namespace_version: NamespaceVersion(1),
        },
        chat_id: ChatId(id),
    }))
    .id()
}

/// A sibling set with distinct identities — the documented precondition of
/// `resolve_siblings`.
fn arb_siblings() -> impl Strategy<Value = Vec<(ItemId, String, NameKind)>> {
    prop::collection::vec((any::<i64>(), arb_raw(), arb_kind()), 1..8).prop_map(|rows| {
        let mut seen = HashSet::new();
        rows.into_iter()
            .filter(|(id, _, _)| seen.insert(*id))
            .map(|(id, raw, kind)| (chat_id(id), raw, kind))
            .collect()
    })
}

/// A sibling set drawn only from [`CASE_VARIANTS`] — short, so that two
/// siblings landing on the same folded name is the common case rather than a
/// lottery win.
fn arb_case_variant_siblings() -> impl Strategy<Value = Vec<(ItemId, String, NameKind)>> {
    let raw = prop::collection::vec(prop::sample::select(CASE_VARIANTS.as_slice()), 1..4)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    prop::collection::vec((any::<i64>(), raw, arb_kind()), 1..6).prop_map(|rows| {
        let mut seen = HashSet::new();
        rows.into_iter()
            .filter(|(id, _, _)| seen.insert(*id))
            .map(|(id, raw, kind)| (chat_id(id), raw, kind))
            .collect()
    })
}

fn resolve(siblings: &[(ItemId, String, NameKind)]) -> Vec<String> {
    let inputs: Vec<SiblingName<'_>> = siblings
        .iter()
        .map(|(id, raw, kind)| SiblingName {
            id,
            raw,
            kind: *kind,
            fixed: false,
        })
        .collect();
    gramdrive_model::naming::resolve_siblings(&inputs)
        .into_iter()
        .map(SafeName::into_string)
        .collect()
}

proptest! {
    /// The whole contract of `sanitize` in one property: whatever goes in,
    /// what comes out is a name every platform accepts.
    #[test]
    fn sanitize_always_produces_a_valid_name(raw in arb_raw(), kind in arb_kind()) {
        let name = sanitize(&raw, kind);
        prop_assert!(
            SafeName::parse(name.as_str()).is_ok(),
            "policy rejected {:?} from {raw:?}",
            name.as_str()
        );
        for platform in Platform::ALL {
            prop_assert_eq!(
                platform.check(name.as_str()),
                Ok(()),
                "{:?} rejected {:?}",
                platform,
                name.as_str()
            );
        }
    }

    /// The traversal acceptance criterion. A name is one component, and no
    /// title can widen it into a path or aim it at a parent directory.
    #[test]
    fn no_input_can_escape_its_component(raw in arb_raw(), kind in arb_kind()) {
        let name = sanitize(&raw, kind);
        let text = name.as_str();
        prop_assert!(!text.contains('/'));
        prop_assert!(!text.contains('\\'));
        prop_assert!(!text.contains('\0'));
        prop_assert!(!text.is_empty());
        prop_assert_ne!(text, ".");
        prop_assert_ne!(text, "..");
        // `..` cannot even be a *part* of a traversal, since a component is
        // never joined with a separator by this layer — but a name that is
        // entirely dots is still a filesystem entry nobody wants.
        prop_assert!(text.chars().any(|character| character != '.'));
    }

    /// Sanitizing an already-sanitized name changes nothing. Without this,
    /// a name that passed through the policy twice — a re-sync, a rebuild —
    /// could drift, and a stable name is the product promise (POL-1).
    #[test]
    fn sanitize_is_idempotent(raw in arb_raw(), kind in arb_kind()) {
        let once = sanitize(&raw, kind);
        let twice = sanitize(once.as_str(), kind);
        prop_assert_eq!(once.as_str(), twice.as_str());
    }

    /// Truncation never overshoots the budget, in either unit.
    #[test]
    fn sanitize_always_fits_the_budget(raw in arb_raw(), kind in arb_kind()) {
        let name = sanitize(&raw, kind);
        prop_assert!(ComponentBudget::strictest().admits(name.as_str()));
    }

    /// A parsed name is already sanitized, so the policy has one fixed point
    /// and `parse` and `sanitize` cannot disagree about what is valid.
    #[test]
    fn parse_accepts_exactly_what_sanitize_produces(raw in arb_raw(), kind in arb_kind()) {
        let name = sanitize(&raw, kind);
        let reparsed = SafeName::parse(name.as_str());
        prop_assert_eq!(reparsed.as_ref().map(SafeName::as_str), Ok(name.as_str()));
    }

    /// SYNC-012: a resolved sibling set is unambiguous on *every* platform,
    /// each folded by its own rule.
    ///
    /// Folded by `Platform::fold`, never by the implementation's own key.
    /// `resolve_siblings` loops until *its* key reports no collisions, so a
    /// test folding the way the implementation folds passes by construction —
    /// it restates the code instead of checking it, which is how a
    /// `to_lowercase` key that merged neither `ΟΔΟΣ`/`οδοσ` nor `ı`/`i`
    /// survived a green suite.
    ///
    /// Re-pointing this property at the platform fixed that, but does *not*
    /// on its own catch the bug: `arb_raw` samples far too wide a space to
    /// land two siblings on the same folded name, so this property still
    /// passes against the broken key (measured, not assumed — see the task's
    /// implementation notes). It proves the invariant over hostile input;
    /// `case_variant_siblings_never_collide` below is what makes it bite.
    #[test]
    fn resolved_siblings_never_collide(siblings in arb_siblings()) {
        let names = resolve(&siblings);
        for platform in Platform::ALL {
            let folded: HashSet<String> =
                names.iter().map(|name| platform.fold(name)).collect();
            prop_assert_eq!(
                folded.len(),
                names.len(),
                "{:?} folds two of {:?} into one directory entry",
                platform,
                names
            );
        }
    }

    /// The same property, aimed — and the one with teeth.
    ///
    /// Every character in the alphabet folds together with another on at
    /// least one platform, so a random pair of short siblings collides
    /// somewhere most of the time. Verified against the defect it exists to
    /// pin: restore the `to_lowercase` key and this fails within a few
    /// hundred cases, while every other property in this file stays green.
    #[test]
    fn case_variant_siblings_never_collide(siblings in arb_case_variant_siblings()) {
        let names = resolve(&siblings);
        for platform in Platform::ALL {
            let folded: HashSet<String> =
                names.iter().map(|name| platform.fold(name)).collect();
            prop_assert_eq!(
                folded.len(),
                names.len(),
                "{:?} folds two of {:?} into one directory entry",
                platform,
                names
            );
        }
    }

    /// SYNC-012, the part that matters: names are a function of the sibling
    /// *set*, not of the order it arrived in. A counter-based suffix fails
    /// exactly here.
    #[test]
    fn resolution_ignores_discovery_order(siblings in arb_siblings()) {
        let forward = resolve(&siblings);

        let mut backward_input = siblings.clone();
        backward_input.reverse();
        let backward = resolve(&backward_input);

        let mapping: HashMap<String, String> = siblings
            .iter()
            .map(|(id, _, _)| id.text())
            .zip(forward)
            .collect();
        let reversed_mapping: HashMap<String, String> = backward_input
            .iter()
            .map(|(id, _, _)| id.text())
            .zip(backward)
            .collect();

        prop_assert_eq!(mapping, reversed_mapping);
    }

    /// Every resolved name is still a valid name — suffixing must not push
    /// one over the budget or reintroduce a reserved stem.
    #[test]
    fn resolved_siblings_are_all_valid_names(siblings in arb_siblings()) {
        for name in resolve(&siblings) {
            prop_assert!(SafeName::parse(&name).is_ok(), "invalid: {:?}", name);
        }
    }

    /// A file keeps its extension through sanitizing and through suffixing.
    /// Losing it makes the file untypeable, and the renderer's links point at
    /// the name (SYNC-032).
    #[test]
    fn files_keep_a_plausible_extension(stem in "[a-z]{1,300}", ext in "[a-z]{1,4}") {
        let raw = format!("{stem}.{ext}");
        let name = sanitize(&raw, NameKind::File);
        prop_assert!(
            name.as_str().ends_with(&format!(".{ext}")),
            "lost .{ext} from {:?}",
            name.as_str()
        );

        // And through a collision.
        let (a, b) = (chat_id(1), chat_id(2));
        let siblings = vec![
            (a, raw.clone(), NameKind::File),
            (b, raw, NameKind::File),
        ];
        for resolved in resolve(&siblings) {
            prop_assert!(resolved.ends_with(&format!(".{ext}")), "lost .{ext} from {resolved:?}");
        }
    }
}

#[test]
fn truncation_that_exposes_a_short_dot_tail_is_a_sanitize_fixed_point() {
    let raw = format!("{}..b{}", "a".repeat(250), "x".repeat(20));

    let once = sanitize(&raw, NameKind::File);
    let twice = sanitize(once.as_str(), NameKind::File);

    assert_eq!(once, twice);
}
