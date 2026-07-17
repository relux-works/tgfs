# TASK-260715-1ffbkg — Safe naming and collision resolution

Implementation notes. Status: ready for review.

Requirements: SYNC-012, SYNC-013, PLAT-021, PLAT-022, POL-1 (DEC-013).

## What landed

`gramdrive_model::naming` (`crates/gramdrive-model/src/naming.rs`) — layer 0,
platform-neutral, per `crates/README.md` ("model owns naming").

| API | Contract |
|---|---|
| `sanitize(raw, kind) -> SafeName` | Total. Any input, however hostile, yields one name every supported platform accepts. |
| `resolve_siblings(&[SiblingName]) -> Vec<SafeName>` | Makes a sibling set collision-free. Suffixes derive from stable identity, never discovery order (SYNC-012). |
| `SafeName::parse(&str)` | Validates against the full policy. The fixed point of `sanitize`. |
| `Platform::{ALL, check, component_budget, case_insensitive}` | Per-platform filesystem rules, modeled faithfully. |
| `chat_folder_name(title, username)` | POL-1 stable form `<Display Name> — @<username>`. |
| `NameKind::{Directory, File}` | Explicit; extension handling must not be guessed. |

## Design decisions

**One name for the strictest target, not one per platform.** The same archive
is read through the macOS File Provider, the Windows CfAPI host, the Android
`DocumentsProvider` and the Linux FUSE adapter. A per-platform name would make
one chat a different path per device and break `chat.json` attachment links
(SYNC-032). So the policy is the *union* of all four rule sets, and
`Platform::check` models each platform faithfully so the corpus can assert
that one output satisfies all four (PLAT-021). The AC's "expected outputs for
Apple/Windows/Android/Linux" is therefore one expectation column asserted
four ways, not four columns.

**Pipeline order is load-bearing.** Strip invisibles → substitute forbidden →
NFC → trim → fallback → truncate → escape reserved. NFC comes *after* the
removals: deleting a control from between a base character and its combining
mark leaves a sequence that composes, so normalizing first emits a non-NFC
name. Pinned by corpus case `control between base and combining mark`.

**ZWJ and ZWNJ are kept** though they sit inside the stripped zero-width
range. Stripping ZWJ (U+200D) tears a family emoji into separate people;
stripping ZWNJ (U+200C) corrupts Persian and Indic text. Bidi overrides,
isolates and LRM/RLM *are* stripped — a spoofing vector, not a display
preference (`photo\u{202e}gnp.exe` renders as `photoexe.png`). This is
GramDrive policy, not filesystem truth: no platform forbids bidi controls,
which is why `Platform::check` does not test for it and `SafeName::parse`
does.

**Truncation cuts at grapheme-cluster boundaries** — "emoji-safe" in the AC.
A codepoint cut halves a flag into a stray regional indicator and leaves
dangling ZWJs. Codepoint fallback exists only for a single cluster larger
than the whole budget (zalgo), and `compose` re-normalizes after that path.
Files keep their extension: losing it makes the file untypeable and breaks
SYNC-032 links.

**Collision suffixes derive from identity, never discovery order (SYNC-012).**
Suffix = base32 of FNV-1a + splitmix64 finalizer over the `ItemId` bytes.
Deliberately *not* an id prefix: sibling ids share long prefixes (format
version, kind tag, account scope) and differ only deep in the payload, so a
prefix would be identical across the very items being distinguished. The
digest is non-cryptographic on purpose — a forced collision is not an attack,
it is just another collision, absorbed deterministically by escalation
(7 base32 chars → 13 → full `ItemId` text, which cannot collide).

Every member of a collision set is suffixed, not just the losers: leaving one
bare privileges an arbitrary member and renames the survivor when the other is
deleted. Detection runs on **final** names, so a title crafted to impersonate
another chat's suffixed name joins the collision set rather than colliding
with it.

**Traversal is impossible by construction.** Separators are substituted before
any structure is derived; `.` and `..` trim to nothing and become `Unnamed`.
`../../etc/passwd` → `.._.._etc_passwd`, one ordinary component. Asserted as a
property over sampled hostile input, not a case list.

**Whole-path budgets stay with the adapters (PLAT-022).** This module budgets
one component (255 UTF-8 bytes / 255 UTF-16 units — the strictest of the four,
derived from `Platform::ALL` rather than hardcoded). The core does not know
where a sync root is mounted, and the layout nests six deep; meeting a 260-char
`MAX_PATH` by component truncation would mangle names on the three platforms
with no such limit. Windows long-path support is the CfAPI host's declared
capability (PLAT-WIN-004).

**One byte held back unconditionally** for the reserved-name escape
underscore. Costs one byte of 255; buys the guarantee that escaping cannot
push a fitted name over budget — no re-truncation loop, no case analysis about
whether truncation just created a `CON`.

## Bug found by the property suite

`unicode_normalization::is_nfc_quick` returns `Maybe` (not `No`) for a string
starting with a combining mark. Treating `!= Yes` as unnormalized rejected
`"\u{301}"` — a lone accent, unusual but perfectly NFC. Caught on the property
suite's first run and minimal-shrunk to the exact input. Fixed by using the
full `is_nfc`; pinned as corpus case `lone combining mark` and as a checked-in
`proptest-regressions` seed (the repo's first).

## Dependencies

First external runtime deps of layer 0: `unicode-normalization`,
`unicode-segmentation` (+ transitive `tinyvec`). UCD tables, not logic — NFC
and grapheme boundaries cannot be hand-written correctly, and a wrong answer
is a name the filesystem silently rewrites or a truncation that splits an
emoji. All **MIT OR Apache-2.0**, inside the POL-6 allow list, so **no
DEC-021-style named exception was needed** — `cargo deny check licenses` is
green as-is. Neither is platform-specific; the architecture check confirms
layer 0 stays platform-neutral.

## Refactor

POL-1 formatting moved `tree.rs::chat_display_name` → `naming::chat_folder_name`
(single source of truth). The tree delegates; output unchanged and the tree
suite passes untouched. The tree still emits **raw** names by design —
sanitization is applied by the consumer over a sibling set, because collision
resolution is set-relative while the tree enumerates lazily per page. That
integration belongs to the provider/adapter layer, not to this task.

## Verification

All commands run in the repo root; all green.

| Command | Result |
|---|---|
| `make check` | **8/8** — toolchain, format, lint, test, architecture, supply-chain, traceability, scripts |
| `cargo test -p gramdrive-model` | **83/83** (37 new) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo deny check licenses` | ok |
| `python3 .scripts/check_crate_architecture.py` | 7 crates conform |

New tests (37):

- `tests/naming_fixture.rs` — 6 tests over a **53-case** shared corpus: one
  expected output per input, each asserted against all four platforms and
  against the policy; plus tests that the per-platform models actually differ
  (otherwise "strictest target" would be a claim about four identical checks),
  that budgets count the unit each platform counts, and that the policy
  rejects what no single platform rejects.
- `tests/naming_collisions.rs` — 15 tests. Suffix goldens pinned like the
  identity encodings (a moved golden is a renamed user folder), order
  independence, case-insensitive collisions, sanitization-induced collisions,
  the crafted-impersonation case, budget/extension interaction with suffixes.
- `tests/naming_properties.rs` — 9 properties over hostile sampled input:
  output always valid, no traversal, idempotence, always fits budget,
  parse/sanitize agree, resolved sets never collide, resolution ignores order,
  resolved names all valid, files keep extensions.

## Notes for review

- `resolve_siblings` precondition: ids must be distinct. Two siblings sharing
  one identity are the same item twice (the tree builder cannot produce it) and
  would get the same name — no identity-derived suffix can separate identical
  identities. Documented on the function.
- Case folding uses `str::to_lowercase`, not full Unicode case folding.
  Adequate and at least as aggressive as the platforms' own tables (a spurious
  collision costs a suffix; a missed one would ship a broken tree). Documented
  as a residual risk on `resolve_siblings`.
- Names are set-relative by nature: they change when the sibling *set* changes,
  never when only the order changes. That is the SYNC-012 guarantee, and it is
  the strongest one available — collision resolution cannot be set-independent.
