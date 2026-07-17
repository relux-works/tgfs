//! Shared cross-platform filename corpus (TASK-260715-1ffbkg; PLAT-021,
//! SYNC-013).
//!
//! One table, one expected output per input, and every case asserted against
//! all four platforms' rules (PLAT-021). The single expectation column *is*
//! the cross-platform contract: SYNC-013 sanitizes for the strictest target,
//! so a per-platform expectation column would mean the same chat resolved to
//! different paths on different devices.
//!
//! The corpus is the executable form of the naming policy. A code change that
//! moves an expectation is changing what a user's folder is called — which is
//! allowed, but never silently.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ChatId, ChatKey, ItemId, ItemKey,
    NamespaceVersion,
};
use gramdrive_model::naming::{
    FALLBACK_NAME, NameKind, Platform, SafeName, SiblingName, chat_folder_name, resolve_siblings,
    sanitize,
};

struct Case {
    /// What the case is for — printed on failure.
    label: &'static str,
    raw: String,
    kind: NameKind,
    expected: String,
}

fn case(label: &'static str, raw: &str, kind: NameKind, expected: &str) -> Case {
    Case {
        label,
        raw: raw.to_string(),
        kind,
        expected: expected.to_string(),
    }
}

/// The corpus. Grouped by the rule each case pins.
fn corpus() -> Vec<Case> {
    use NameKind::{Directory, File};

    let mut cases = vec![
        // --- Ordinary names survive untouched ---------------------------
        case("plain title", "Team Chat", Directory, "Team Chat"),
        case("plain file", "photo.jpg", File, "photo.jpg"),
        case("non-latin title", "Семейный чат", Directory, "Семейный чат"),
        case("cjk title", "家族チャット", Directory, "家族チャット"),
        // --- POL-1 chat folder form (DEC-013) ---------------------------
        case(
            "pol-1 title with username",
            &chat_folder_name("Alex", Some("alex_dev")),
            Directory,
            "Alex — @alex_dev",
        ),
        case(
            "pol-1 title without username",
            &chat_folder_name("Alex", None),
            Directory,
            "Alex",
        ),
        // --- Path separators and traversal (AC: no traversal) -----------
        case("forward slash", "reports/2026", Directory, "reports_2026"),
        case(
            "backslash",
            "C:\\Windows\\System32",
            Directory,
            "C__Windows_System32",
        ),
        case(
            "relative traversal",
            "../../etc/passwd",
            Directory,
            ".._.._etc_passwd",
        ),
        case(
            "absolute posix path",
            "/etc/shadow",
            Directory,
            "_etc_shadow",
        ),
        case("dot segment", ".", Directory, FALLBACK_NAME),
        case("parent segment", "..", Directory, FALLBACK_NAME),
        case("many dots", "...", Directory, FALLBACK_NAME),
        // --- Windows forbidden characters -------------------------------
        case(
            "every forbidden character",
            "a<b>c:d\"e|f?g*h",
            Directory,
            "a_b_c_d_e_f_g_h",
        ),
        // --- Control characters -----------------------------------------
        case("bell control", "Chat\u{7}Name", Directory, "ChatName"),
        case("nul byte", "a\0b", Directory, "ab"),
        case("del and c1", "a\u{7f}b\u{85}c", Directory, "abc"),
        case("zero-width space", "a\u{200b}b", Directory, "ab"),
        case("byte order mark", "\u{feff}Chat", Directory, "Chat"),
        // --- Bidi spoofing ----------------------------------------------
        // Rendered with the override intact, this reads "photoexe.png".
        case(
            "right-to-left override",
            "photo\u{202e}gnp.exe",
            File,
            "photognp.exe",
        ),
        case("bidi isolate", "a\u{2066}b\u{2069}c", Directory, "abc"),
        // --- Unicode normalization (NFC) --------------------------------
        case("decomposed acute", "Cafe\u{301}", Directory, "Café"),
        // The control sits between the base and its mark: stripping it first
        // is what lets NFC compose. Normalizing first would emit "e" + mark.
        case(
            "control between base and combining mark",
            "Cafe\u{7}\u{301}",
            Directory,
            "Café",
        ),
        case(
            "decomposed hangul",
            "\u{1100}\u{1161}\u{11a8}",
            Directory,
            "\u{ac01}",
        ),
        // A combining mark with nothing to combine with. Odd text, but
        // already NFC — the quick normalization check answers "maybe" here,
        // and reading maybe as no would reject a name that is perfectly fine
        // (found by the property suite).
        case("lone combining mark", "\u{301}", Directory, "\u{301}"),
        // --- Trailing dots and spaces (Windows drops them) --------------
        case("trailing dots", "Chat...", Directory, "Chat"),
        case("trailing spaces", "Chat   ", Directory, "Chat"),
        case("leading spaces", "   Chat", Directory, "Chat"),
        case("trailing dot on file", "notes.txt.", File, "notes.txt"),
        // Interior dots are legal everywhere and are left alone.
        case("interior dots", "v1.2.3 notes", Directory, "v1.2.3 notes"),
        // A leading dot is a legitimate title; it only hides the folder on
        // the POSIX platforms, which is display, not correctness.
        case("leading dot", ".config", Directory, ".config"),
        // --- Empty results ----------------------------------------------
        case("empty title", "", Directory, FALLBACK_NAME),
        case("whitespace only", "   ", Directory, FALLBACK_NAME),
        case("controls only", "\u{7}\u{8}", Directory, FALLBACK_NAME),
        // --- Windows reserved device names ------------------------------
        case("reserved con", "CON", Directory, "CON_"),
        case("reserved lowercase", "con", Directory, "con_"),
        case("reserved with extension", "CON.txt", File, "CON_.txt"),
        // As a directory the whole string is the stem, but Windows still
        // reads CON before the first dot — the escape must land there.
        case(
            "reserved dotted directory",
            "CON.txt",
            Directory,
            "CON_.txt",
        ),
        case("reserved com1", "COM1", Directory, "COM1_"),
        case("reserved lpt9", "LPT9", Directory, "LPT9_"),
        case("reserved nul", "NUL", Directory, "NUL_"),
        case("reserved conout", "CONOUT$", Directory, "CONOUT$_"),
        case("reserved with trailing space", "CON ", Directory, "CON_"),
        // Merely starting with a device name is not reserved.
        case("console is not con", "CONSOLE", Directory, "CONSOLE"),
        case("com10 is not com1", "COM10", Directory, "COM10"),
        // --- Emoji are ordinary text ------------------------------------
        case("emoji title", "Trip 🏖️ 2026", Directory, "Trip 🏖️ 2026"),
        case("zwj family emoji", "👨‍👩‍👧‍👦", Directory, "👨‍👩‍👧‍👦"),
        // ZWNJ carries meaning in Persian; stripping it corrupts the word.
        case(
            "zwnj is kept",
            "می\u{200c}خواهم",
            Directory,
            "می\u{200c}خواهم",
        ),
    ];

    // --- Length budget: 255 bytes, minus one held back for a possible
    // reserved-name escape (see `compose`).
    let budget = 254;
    cases.push(case(
        "over-long ascii truncates to the budget",
        &"a".repeat(300),
        Directory,
        &"a".repeat(budget),
    ));
    // 63 x 4 bytes = 252; a 64th would be 256 and overshoot.
    cases.push(case(
        "over-long emoji truncates whole codepoints",
        &"😀".repeat(100),
        Directory,
        &"😀".repeat(63),
    ));
    // One family is one grapheme cluster of 25 bytes: 10 fit in 250 bytes,
    // and a byte-wise or codepoint-wise cut would leave a lone 👨 or a
    // dangling ZWJ at the end.
    cases.push(case(
        "over-long zwj sequence truncates whole clusters",
        &"👨‍👩‍👧‍👦".repeat(20),
        Directory,
        &"👨‍👩‍👧‍👦".repeat(10),
    ));
    // A flag is two regional indicators — 8 bytes, one cluster. Half a flag
    // is a stray letter.
    cases.push(case(
        "over-long flags truncate whole clusters",
        &"🇺🇸".repeat(50),
        Directory,
        &"🇺🇸".repeat(31),
    ));
    // The extension is what makes a file openable; truncation keeps it and
    // spends the budget on the stem.
    cases.push(case(
        "over-long file keeps its extension",
        &format!("{}.jpg", "b".repeat(300)),
        File,
        &format!("{}.jpg", "b".repeat(budget - 4)),
    ));

    cases
}

#[test]
fn corpus_matches_expected_output() {
    for Case {
        label,
        raw,
        kind,
        expected,
    } in corpus()
    {
        let actual = sanitize(&raw, kind);
        assert_eq!(actual.as_str(), expected, "case '{label}'");
    }
}

#[test]
fn every_expected_output_is_accepted_by_every_platform() {
    for Case {
        label,
        raw,
        kind,
        expected: _,
    } in corpus()
    {
        let name = sanitize(&raw, kind);
        for platform in Platform::ALL {
            assert_eq!(
                platform.check(name.as_str()),
                Ok(()),
                "case '{label}' rejected by {platform:?}: {name}"
            );
        }
        // The policy is strictly stronger than any single platform.
        assert!(
            SafeName::parse(name.as_str()).is_ok(),
            "case '{label}' fails the policy: {name}"
        );
    }
}

#[test]
fn no_corpus_output_can_escape_its_component() {
    for Case {
        label, raw, kind, ..
    } in corpus()
    {
        let name = sanitize(&raw, kind);
        let text = name.as_str();
        assert!(!text.contains('/'), "case '{label}' kept a slash: {text}");
        assert!(
            !text.contains('\\'),
            "case '{label}' kept a backslash: {text}"
        );
        assert!(!text.is_empty(), "case '{label}' produced an empty name");
        assert_ne!(text, ".", "case '{label}' produced a dot segment");
        assert_ne!(text, "..", "case '{label}' produced a parent segment");
    }
}

// ---------------------------------------------------------------------------
// Case-folding corpus (PLAT-021, SYNC-012)
// ---------------------------------------------------------------------------

/// Two chat titles, and the platforms on which they name the *same* directory
/// entry once sanitized.
///
/// The `merged_by` column is hand-written filesystem truth — NTFS's `$UpCase`
/// table, APFS's Unicode case folding, ext4's byte comparison — not a
/// prediction of what the code does. That is the point of the column: the
/// sanitize corpus above asserts "one output satisfies all four platforms",
/// which structurally cannot notice a *folding* mismatch, because folding is
/// about pairs and that table has one name per row.
struct FoldCase {
    label: &'static str,
    a: &'static str,
    b: &'static str,
    /// Platforms that resolve `a` and `b` to one entry. Empty means the two
    /// titles are distinct everywhere and neither needs a suffix.
    merged_by: &'static [Platform],
}

fn fold_case(
    label: &'static str,
    a: &'static str,
    b: &'static str,
    merged_by: &'static [Platform],
) -> FoldCase {
    FoldCase {
        label,
        a,
        b,
        merged_by,
    }
}

/// Every pair the platforms disagree about, and a few they agree on.
///
/// Note how little of this is "uppercase versus lowercase": the sigmas need
/// three-way folding, `ı`/`i` collide only on Windows, `ẞ`/`ß` only on Apple,
/// and Kelvin and Angstrom are settled by NFC before folding is even reached.
fn fold_corpus() -> Vec<FoldCase> {
    const BOTH: &[Platform] = &[Platform::Apple, Platform::Windows];
    const EVERY: &[Platform] = &Platform::ALL;
    const APPLE: &[Platform] = &[Platform::Apple];
    const WINDOWS: &[Platform] = &[Platform::Windows];
    const NONE: &[Platform] = &[];

    vec![
        // --- Latin, the easy case both platforms agree on ---------------
        fold_case("latin case difference", "Bob", "BOB", BOTH),
        fold_case("distinct titles collide nowhere", "Alice", "Bob", NONE),
        // --- Greek: the sigma has three forms and two of them are the
        // same letter. NTFS uppercases all three to Σ; APFS folds all three
        // to σ. A lowercasing fold keeps final ς apart from medial σ and so
        // hands both chats the same folder.
        fold_case("greek caps vs lowercase", "ΟΔΟΣ", "οδοσ", BOTH),
        fold_case("greek final vs medial sigma", "οδος", "οδοσ", BOTH),
        fold_case("greek caps vs final sigma", "ΟΔΟΣ", "οδος", BOTH),
        // --- Turkish dotless i: Windows-only. `$UpCase` sends ı and i alike
        // to I; Unicode case folding leaves them as themselves, so this pair
        // is one folder on NTFS and two on APFS.
        fold_case("turkish dotless i vs latin i", "ı", "i", WINDOWS),
        // The dotted capital İ is nobody's collision: uppercasing leaves it,
        // and folding sends it to `i` + combining dot, not to `i`.
        fold_case("turkish dotted capital i vs latin i", "İ", "i", NONE),
        // --- Sharp s: Apple-only, and the pair that rules out the
        // to_uppercase().to_lowercase() round trip. APFS folds ẞ and ß alike
        // to `ss`; the round trip sends ß to `ss` but ẞ only back to ß.
        fold_case("capital sharp s vs sharp s", "\u{1e9e}", "ß", APPLE),
        fold_case("capital sharp s vs ss", "\u{1e9e}", "ss", APPLE),
        // NTFS reaches this one only because our model uppercases with full
        // mappings (ß -> SS) where `$UpCase` would not; stricter than the
        // platform, which costs a suffix and never a shadowed folder.
        fold_case("sharp s vs ss", "ß", "ss", BOTH),
        // --- Ligature: folding and uppercasing both decompose it ---------
        fold_case("fi ligature vs fi", "\u{fb01}", "fi", BOTH),
        // --- Compatibility singletons: NFC settles these before folding,
        // so they are one name even on the case-sensitive platforms.
        fold_case("kelvin sign vs latin k", "\u{212a}", "K", EVERY),
        fold_case("angstrom sign vs a-ring", "\u{212b}", "\u{c5}", EVERY),
    ]
}

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(42),
        },
        namespace_version: NamespaceVersion(1),
    }
}

fn chat_id(id: i64) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
        scope: scope(),
        chat_id: ChatId(id),
    }))
    .id()
}

#[test]
fn fold_corpus_matches_each_platform_model() {
    for FoldCase {
        label,
        a,
        b,
        merged_by,
    } in fold_corpus()
    {
        let (left, right) = (
            sanitize(a, NameKind::Directory),
            sanitize(b, NameKind::Directory),
        );
        for platform in Platform::ALL {
            let merged = platform.fold(left.as_str()) == platform.fold(right.as_str());
            assert_eq!(
                merged,
                merged_by.contains(&platform),
                "case '{label}' on {platform:?}: {left} and {right} merged={merged}"
            );
        }
        // A case-sensitive platform merges only names that are already equal;
        // if this ever fails, `fold` has grown a rule ext4 does not have.
        for platform in [Platform::Linux, Platform::Android] {
            assert_eq!(
                platform.fold(left.as_str()) == platform.fold(right.as_str()),
                left == right,
                "case '{label}': {platform:?} is not comparing bytes"
            );
        }
    }
}

#[test]
fn colliding_titles_are_resolved_apart_on_every_platform() {
    for FoldCase {
        label,
        a,
        b,
        merged_by,
    } in fold_corpus()
    {
        let (x, y) = (chat_id(1), chat_id(2));
        let names: Vec<String> = resolve_siblings(&[
            SiblingName {
                id: &x,
                raw: a,
                kind: NameKind::Directory,
                fixed: false,
            },
            SiblingName {
                id: &y,
                raw: b,
                kind: NameKind::Directory,
                fixed: false,
            },
        ])
        .into_iter()
        .map(SafeName::into_string)
        .collect();

        // The contract: whatever any platform would have merged, resolution
        // has pulled apart — checked against the platform's own fold, not
        // against the key `resolve_siblings` used to decide.
        for platform in Platform::ALL {
            assert_ne!(
                platform.fold(&names[0]),
                platform.fold(&names[1]),
                "case '{label}': {:?} still collide on {platform:?}",
                names
            );
        }

        // And the suffix is spent only where it is owed.
        let suffixed = names.iter().filter(|name| name.contains(" (")).count();
        if merged_by.is_empty() {
            assert_eq!(suffixed, 0, "case '{label}': paid a suffix for nothing");
        } else {
            assert_eq!(
                suffixed, 2,
                "case '{label}': both members of a collision get a suffix, got {names:?}"
            );
        }
    }
}

/// The regression the review caught, pinned as a golden rather than as a
/// property: two ordinary Greek chats, one titled in capitals and one in
/// lowercase, must not share a folder.
///
/// Goldens because a suffix is part of a user's folder name (see the module
/// docs of `naming_collisions.rs`): moving one renames a real directory.
#[test]
fn greek_sigma_and_dotless_i_pairs_have_pinned_suffixes() {
    let (x, y) = (chat_id(1), chat_id(2));
    let resolve = |a: &str, b: &str| -> Vec<String> {
        resolve_siblings(&[
            SiblingName {
                id: &x,
                raw: a,
                kind: NameKind::Directory,
                fixed: false,
            },
            SiblingName {
                id: &y,
                raw: b,
                kind: NameKind::Directory,
                fixed: false,
            },
        ])
        .into_iter()
        .map(SafeName::into_string)
        .collect()
    };

    assert_eq!(
        resolve("ΟΔΟΣ", "οδοσ"),
        vec!["ΟΔΟΣ (47fjxm4)", "οδοσ (27ngzyb)"]
    );
    assert_eq!(
        resolve("οδος", "οδοσ"),
        vec!["οδος (47fjxm4)", "οδοσ (27ngzyb)"]
    );
    assert_eq!(resolve("ı", "i"), vec!["ı (47fjxm4)", "i (27ngzyb)"]);
    assert_eq!(
        resolve("\u{1e9e}", "ß"),
        vec!["\u{1e9e} (47fjxm4)", "ß (27ngzyb)"]
    );
}

#[test]
fn platform_checks_model_each_platform_faithfully() {
    // The corpus proves the *output* satisfies everyone. These pin that the
    // per-platform models actually differ — otherwise "strictest target"
    // would be a claim about four identical checks.
    let colon = "a:b";
    assert!(Platform::Windows.check(colon).is_err());
    assert!(Platform::Linux.check(colon).is_ok());
    assert!(Platform::Apple.check(colon).is_ok());

    let reserved = "CON";
    assert!(Platform::Windows.check(reserved).is_err());
    assert!(Platform::Linux.check(reserved).is_ok());

    let trailing = "Chat.";
    assert!(Platform::Windows.check(trailing).is_err());
    assert!(Platform::Android.check(trailing).is_ok());

    // Only Apple and Windows resolve names case-insensitively.
    assert!(Platform::Apple.case_insensitive());
    assert!(Platform::Windows.case_insensitive());
    assert!(!Platform::Linux.case_insensitive());
    assert!(!Platform::Android.case_insensitive());

    // Every platform rejects the separator, NUL, and the dot segments.
    for platform in Platform::ALL {
        assert!(platform.check("a/b").is_err(), "{platform:?}");
        assert!(platform.check("a\0b").is_err(), "{platform:?}");
        assert!(platform.check(".").is_err(), "{platform:?}");
        assert!(platform.check("..").is_err(), "{platform:?}");
        assert!(platform.check("").is_err(), "{platform:?}");
    }
}

#[test]
fn platform_budgets_count_the_unit_each_platform_counts() {
    // 200 astral characters: 800 UTF-8 bytes, 400 UTF-16 units. Over the
    // limit on every platform, but for different reasons — which is why the
    // budget carries both units.
    let astral = "𝄞".repeat(200);
    assert!(matches!(
        Platform::Linux.check(&astral),
        Err(gramdrive_model::naming::NameViolation::TooLongUtf8 { .. })
    ));
    assert!(matches!(
        Platform::Windows.check(&astral),
        Err(gramdrive_model::naming::NameViolation::TooLongUtf16 { .. })
    ));

    // 200 BMP characters that cost 3 bytes each: 600 bytes, 200 units. Over
    // the POSIX byte limit, comfortably inside the NTFS unit limit.
    let cjk = "家".repeat(200);
    assert!(Platform::Linux.check(&cjk).is_err());
    assert!(Platform::Windows.check(&cjk).is_ok());
}

#[test]
fn policy_rejects_what_no_single_platform_rejects() {
    // Invisible characters and non-NFC forms pass every platform's own rules
    // and still fail the policy — the difference between filesystem truth
    // and GramDrive's naming contract.
    use gramdrive_model::naming::NameViolation;

    let bidi = "photo\u{202e}gnp.exe";
    assert!(Platform::Linux.check(bidi).is_ok());
    assert!(matches!(
        SafeName::parse(bidi),
        Err(NameViolation::InvisibleCharacter { .. })
    ));

    let decomposed = "Cafe\u{301}";
    assert!(Platform::Linux.check(decomposed).is_ok());
    assert_eq!(
        SafeName::parse(decomposed),
        Err(NameViolation::NotNormalized)
    );
}
