# gramdrive-model

Domain vocabulary of the GramDrive core: item identity, the virtual
`chat -> folder -> files` tree, naming/sanitization policy, versions, change
cursors, byte ranges, and the SHA-256 content hash that names blobs. Layer 0 —
every other crate depends on it; it depends on nothing inside the workspace.

The `hash` module is a self-contained, streaming SHA-256 (FIPS 180-4),
vendored rather than pulled from a crate to keep this platform-neutral layer
free of a hashing crate's build scripts and transitive tree (POL-6,
`deny.toml` `[bans.build]`). It is pinned to the FIPS 180-4 and NIST
known-answer vectors and hashes already-public content for identity and
integrity only — not secrets — so it makes no constant-time claim.

## Ownership

STORY-260715-3qxar5 (identity-and-namespace), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1qz1g5 (stable item identities),
TASK-260715-3tjduq (virtual tree builder), TASK-260715-1ffbkg (cross-platform
naming), TASK-260715-1jmsdp (ordering projection), and TASK-260715-1j4ij3
(versions and change cursors, for the drive-source contract —
STORY-260715-255sa3).

## Dependencies

Internal: none. Platform-specific code: forbidden. See `crates/README.md`.

External: `unicode-normalization`, `unicode-segmentation` and `caseless` —
Unicode character data for the naming policy (NFC, grapheme-cluster
boundaries, full case folding). Tables, not logic: they cannot be
hand-written correctly, and a wrong answer is a name the filesystem silently
rewrites, a truncation that splits an emoji, or two chats that collapse into
one folder. `caseless` carries the CaseFolding data the other two do not:
APFS folds sibling names by full Unicode case folding, which no combination
of `to_lowercase`/`to_uppercase` reproduces (`ẞ` folds to `ss`, which neither
mapping reaches) — see `Platform::fold`. Licenses are MIT OR Apache-2.0 and
MIT, all inside the POL-6 allow list, so none needs a DEC-021-style named
exception. None is platform-specific — they are the data each platform's own
rules are written in — so layer 0 stays platform-neutral.

Dev-only: `proptest` (identity, tree, naming and ordering property suites);
it must never move to `[dependencies]` — test-support code does not ship
(crates/README.md).

## Stable item identities (DEC-008, DOM-001..DOM-024)

The `identity` module owns the typed keys and their opaque serialization —
TASK-260715-1qz1g5. The model:

- **Canonical keys** (`CanonicalKey`) name source-derived records: account,
  chat list (Main/Archive/folder), the folder catalog, chat, chat-export
  year and media directories, message, attachment, generated document
  (NDJSON/Markdown/JSON), the per-list ordering document (`order.json`), and
  content-addressed blob. Telegram-derived keys
  are scoped by `AccountScope` = account + `NamespaceVersion` (the epoch that
  retires an account's whole derived namespace at once, DOM-021).
- **Appearance keys** (`AppearanceKey`) name one *virtual appearance* of a
  canonical item through a chat-list view. One chat in Main, Archive, and a
  folder is three appearances over one canonical key (DOM-002, DOM-022,
  PRD-013). Appearances cannot nest.
- **`ItemId`** is the opaque, versioned serialization of exactly one key —
  the single namespace every provider resolves through (DOM-024): text form
  for Apple item identifiers and Android document IDs, binary form for
  Windows file identity payloads, and the base of the Linux inode mapping.
- No key type carries a string or an ordering position, so titles, paths,
  filenames, and display order cannot influence identity by construction
  (DOM-001, DOM-005).

### Serialization format v1

Opaque means consumers must not interpret the bytes; the format is still
specified, because the specification is what freezing it means.

Binary form (all integers big-endian, two's complement):

| Offset | Field |
|---|---|
| 0 | format version, `0x01` |
| 1 | item kind tag |
| 2.. | that kind's fields, fixed order |

Item kind tags: account `0x01` (account id: i64), chat list `0x02` (scope,
list kind), chat `0x03` (scope, chat id: i64), message `0x04` (chat fields,
message id: i64), attachment `0x05` (message fields, index: u32), generated
document `0x06` (chat fields, partition, format, schema family: u16), blob
`0x07` (account id: i64, hash), folder catalog `0x08` (scope), year
directory `0x09` (chat fields, year: u16), media directory `0x0a` (chat
fields, year: u16), order document `0x0b` (scope, list kind, schema family:
u16), appearance `0x10` (list kind, then the wrapped canonical key's tag and
fields). Scope = account id (i64) + namespace
version (u32). List kind: Main `0x01`, Archive `0x02`, Folder `0x03` +
folder id (i32). Partition: chat `0x01`, year `0x02` + u16, month `0x03` +
u16 + u8. Format: NDJSON `0x01`, Markdown `0x02`, JSON `0x03`. Hash:
SHA-256 `0x01` + 32 digest bytes.

The directory kinds (`0x08`–`0x0a`) and the JSON format tag were added by
the virtual tree builder (TASK-260715-3tjduq), and the order document
(`0x0b`) by the ordering projection (TASK-260715-1jmsdp), in the extension
room the v1 canonical range reserved for them. The additions are purely
additive: no pre-existing encoding changed, and the original golden fixtures
pin that.

Every field is fixed-width once its tags are read, so the encoding is a
prefix code: decoding is deterministic, no valid encoding is a prefix of
another, and parsing consumes the whole payload or fails.

Text form: `"gd"` + unpadded lowercase RFC 4648 base32 of the binary form.
Parsing is strict — lowercase only, no padding, zero trailing bits — so each
key has exactly one valid text spelling.

### Version policy

v1 is frozen by golden fixtures (`tests/identity_golden.rs`): any change
that alters an encoding fails them, and the correct response is a new format
version byte decoded alongside v1, never a mutation of v1. Every non-`0x01`
version byte fails with `UnsupportedVersion` today, which is what lets a
future format coexist without ambiguity. Ids must stay parseable by every
future app version (DOM-020).

### Collision behavior

Distinct keys cannot share an `ItemId`: decoding is a function and every key
round-trips, so equal encodings force equal keys. The property suite
(`tests/identity_properties.rs`) proves the round-trip and samples distinct
pairs directly. Residual collision surface lives in the inputs:

- **Blob hashes** — two different byte streams sharing a SHA-256 digest
  would collide; accepted on the hash's collision resistance. The algorithm
  tag makes a future hash a new identity space, not a migration.
- **Telegram ID reuse** — the same numeric IDs meaning different objects
  after re-authorization is exactly what the `NamespaceVersion` bump
  retires.
- **Display-name collisions** are not identity collisions; deterministic
  suffixing is naming policy (SYNC-012, TASK-260715-1ffbkg).

## Versions and change cursors (DOM-003, DOM-004, SYNC-004)

The `version` and `cursor` modules own the durable sync vocabulary —
TASK-260715-1j4ij3. They live in this crate rather than `gramdrive-source`
because the state store persists both and may depend only on layer 0.

- **`MetadataVersion` / `ContentVersion`** are opaque, provider-chosen
  tokens compared for equality only (DOM-003 allows monotonic *or*
  content-derived versions, so cross-token ordering is meaningless and
  `Ord` is deliberately absent). Two distinct types, not one: a metadata
  version where a content version belongs is the mistake that publishes
  stale bytes under a fresh stamp, so the type system refuses it.
  Validation: non-empty, ≤ 256 bytes, no control characters. The durable
  form is the token text itself.
- **`ChangeCursor`** anchors one position in a source's change feed. It
  carries the `AccountScope` it was minted under plus an opaque provider
  payload (≤ 4096 bytes); `require_scope` rejects account/namespace
  mismatches explicitly (SYNC-004), and decoding rejects unknown format
  versions distinctly from corruption.

### Cursor serialization format v1

Binary layout (integers big-endian, two's complement): byte 0 format
version (`0x01`); bytes 1..9 account id (i64); bytes 9..13 namespace
version (u32); bytes 13.. the provider payload (the remainder — exactly one
variable-length field, at the tail, so the encoding is injective). Text
form: `"gdc-"` + unpadded lowercase base32 of the binary form — the same
strict codec as identity text (shared `base32` module), so each cursor has
exactly one valid spelling. The prefixes cannot alias: cursor text fails
`ItemId` parsing at the `-`, identity text fails cursor parsing at the
missing prefix.

v1 is frozen by golden fixtures (`tests/cursor_golden.rs`), same policy as
identity: a change that breaks them is wrong; evolution is a new format
version byte decoded alongside v1. The format carries no checksum on
purpose — the payload is opaque provider state; corruption *detection* is
not a cursor guarantee, scope and schema rejection are
(`tests/cursor_properties.rs` proves round-trip, injectivity, canonicality,
and totality of parsing).

## Virtual tree builder (SYNC-010..012, PRD-010..013, DEC-007)

The `tree` module owns `TreeProjection` — TASK-260715-3tjduq. It projects
normalized source records into the default layout of
`.spec/sync-and-filesystem-semantics.md`:

```text
Account/                      canonical account key
  Main/                       canonical chat-list key
    order.json                canonical order-document key (POL-1)
    Chat/                     appearance (view × canonical chat)
      chat.json               appearance over generated doc (JSON, whole chat)
      messages.ndjson         appearance over generated doc (NDJSON, whole chat)
      2026/                   appearance over year-directory key
        07.md                 appearance over generated doc (Markdown, month)
        media/                appearance over media-directory key
          <attachment files>  appearances over attachment keys
  Archive/
    order.json
  Telegram Folders/           canonical folder-catalog key
    <one dir per folder>      canonical chat-list keys (folder kind, each
                              with its own order.json)
```

The model:

- **One record, many appearances (PRD-013).** A projection stores exactly
  one canonical record per chat; views hold references. Everything below a
  view root is an appearance identity — the view wrapped around the
  unchanged canonical key — so no canonical record or blob identity is
  ever duplicated. Which `(view, item)` combinations resolve is this
  module's discipline: chats and their subtrees appear through views;
  accounts, chat lists, the catalog, messages, and blobs never do.
- **Lazy, paged enumeration (SYNC-003).** `children` mints only the
  requested page of the requested parent. Page boundaries are the last
  returned child's `ItemId`; within one projection pages are repeatable
  with no duplicates or gaps, and a boundary from another snapshot fails
  loudly rather than skipping children.
- **Determinism.** Sibling order derives from stable identity (fixed
  roots, folder IDs, chat IDs, years, months, message/attachment
  ordinals), never input order — the property suite
  (`tests/tree_properties.rs`) shuffles every input collection and
  requires identical output; `tests/tree_fixture.rs` pins the spec's
  layout example literally.
- **Read-only capabilities (DEC-007, SYNC-060).** Every node carries
  capability metadata whose write side is constant `false`; v1
  constructors cannot express anything else.
- Display names are raw presentation state in the POL-1 stable form
  (`naming::chat_folder_name`); sanitization and collision suffixing are
  the `naming` module below, applied by the consumer over a sibling set.
- **Sibling order is not Telegram's order, and no name carries a position**
  (POL-1, DEC-013). Each list root publishes its exact order as an
  `order.json` child instead, whose bytes are the `ordering` module's;
  a reorder rewrites that one document and changes nothing in this tree.

## Cross-platform naming (SYNC-012, SYNC-013, PLAT-021, POL-1)

The `naming` module projects untrusted Telegram titles onto filesystem
names — TASK-260715-1ffbkg. `sanitize` is total (no input has a failure
mode) and `resolve_siblings` makes a sibling set collision-free.

- **One name for the strictest target.** Not one name per platform: the
  same archive is read through the macOS, Windows, Android and Linux
  adapters, and a per-platform name would make one chat a different path
  per device and break `chat.json` links (SYNC-032). The policy is the
  union of all four platforms' rules; `Platform::check` models each
  platform faithfully so the corpus can assert one output satisfies all
  of them (PLAT-021).
- **The pipeline** (order is load-bearing, see the module docs): remove
  invisible characters (controls, bidi overrides, ZWSP/BOM — but never
  ZWJ/ZWNJ, which carry meaning); substitute the Windows forbidden set
  `< > : " / \ | ? *` with `_`; normalize to NFC *after* the removals, since
  deleting a control from between a base and its combining mark can leave a
  sequence that composes; trim leading whitespace and trailing dots/spaces
  (Windows drops them silently); fall back to `Unnamed` if nothing is left;
  truncate to 255 bytes / 255 UTF-16 units at grapheme-cluster boundaries;
  escape Windows device names on the stem before the first dot
  (`CON.txt` -> `CON_.txt`).
- **Traversal is impossible by construction.** Separators are substituted
  before any structure is derived, and `.`/`..` trim to nothing and become
  the fallback. A property test asserts it over sampled hostile input, not
  a case list.
- **Collision suffixes derive from stable identity, never discovery order**
  (SYNC-012). `Bob (k3m9xq2)` — base32 of a mixed digest of the `ItemId`
  bytes, not a prefix of the id (sibling ids share long prefixes and would
  give identical suffixes). A counter would renumber folders on every
  re-sync. Every member of a collision set is suffixed, and the check runs
  on final names, so a title crafted to impersonate another chat's suffixed
  name simply joins the collision set. Escalation ends at the full `ItemId`
  text, which cannot collide.
- **Fixed names are the one asymmetry** (`SiblingName::fixed`, POL-1). A
  sibling whose name is GramDrive's own constant rather than a title —
  `order.json` at a list root — keeps it, and colliding titles yield. The two
  reasons suffixing is otherwise symmetric do not apply to a constant: it
  privileges nothing arbitrary, and it cannot be deleted, so no survivor is
  renamed by its disappearance. Suffixing it would move the name POL-1
  publishes, or hand a provider two children called `order.json`.
- **Collisions are folded through every platform, not through one.**
  `Platform::fold` models how each platform decides two names are one entry:
  Windows by NTFS's `$UpCase` (uppercase), Apple by full Unicode case
  folding, Android/Linux by bytes. Neither case-insensitive fold contains the
  other — `ı`/`i` collide only on Windows, Kelvin `K`/`K` only on Apple, and
  `ẞ`/`ß` only on Apple — so the collision key composes all four. No stock
  mapping is a substitute: `to_lowercase` misses every Windows-only pair
  (and shipped that bug), `to_uppercase` every Apple-only one, and the round
  trip still misses `ẞ`/`ß`.
- **Whole-path budgets are the adapters' (PLAT-022).** This module budgets
  one component; the core does not know where a sync root is mounted, and
  meeting a 260-char `MAX_PATH` by component truncation would mangle names
  on the three platforms with no such limit. Windows long-path support is
  the CfAPI host's declared capability (PLAT-WIN-004).

Tests: `tests/naming_fixture.rs` is the shared corpus — one expected output
per input asserted against all four platforms, plus a fold corpus of *pairs*
with the platforms that merge each (a one-name-per-row table cannot catch a
wrong fold: every row passes while two rows name one folder);
`tests/naming_collisions.rs` pins the suffix goldens and the
order-independence of resolution; `tests/naming_properties.rs` proves the
invariants over sampled hostile input and, in
`case_variant_siblings_never_collide`, over an alphabet of nothing but
characters the platforms fold differently. The property and corpus suites
fold by `Platform::fold`, never by the implementation's key — a test that
folds the way the code folds passes by construction.

## Ordering projection (POL-1, DEC-013, SYNC-011)

The `ordering` module publishes Telegram's exact dialog order as data —
TASK-260715-1jmsdp. Filesystems sort by name and Telegram does not, so POL-1
keeps folder names stable and writes the order to `order.json` at each
chat-list root (Main, Archive, and every custom folder), regenerated on
reorder events.

- **A reorder is a content change, never a rename.** When only positions
  change, every `ItemId`, every folder name and every path is untouched; the
  only thing that changes is the bytes of `order.json`. Nothing is keyed by
  order, so nothing moves and no cached content is invalidated. A folder name
  changes on exactly one input: the chat's own title or username changing.
- **One mode, not two** (DEC-013). The numeric-prefix mode that SYNC-011 and
  PRD-012 floated (`001 — Alex/`) is out of scope for v1, and is therefore
  absent rather than present-and-disabled: a mode that ships switched off is
  untested code that reads as a supported feature. Revisiting it post-v1
  means a new decision row and a new projection mode, not re-enabling
  something dormant.
- **Ordering rule.** Chats sort by `(order, chat_id)` descending — Telegram's
  own rule for `chatPosition`, and a total order because chat IDs are unique
  within a list. Total is what makes the document a pure function of the
  input set: shuffled records render byte-identical bytes, with no tie left
  for input order to settle. `is_pinned` is recorded but is *not* a sort key;
  Telegram already encodes pinning in `order`, and sorting by it again would
  be a second, disagreeing implementation of the server's ranking.
- **The document names real directories.** Entries carry the sanitized,
  collision-suffixed name actually projected on disk, resolved over the list
  root's whole sibling set — including `order.json` itself, which is why the
  document cannot be shadowed by a chat titled `order.json`.
- **Identity.** `OrderDocKey` = (chat list, schema family). Keyed by the list
  it describes, not by the order it records, so a reorder cannot touch it.

### `order.json` schema

Deterministic by construction: fixed field order, no timestamps, no host
state, no map iteration. Equal projections render byte-identical documents,
so a sync that changed nothing rewrites nothing.

```json
{
  "schema": "gramdrive.order",
  "schema_family": 1,
  "list": { "kind": "main" },
  "chats": [
    {
      "rank": 0,
      "id": "gdaeiacayaaaaaaaaaaavaaaaaaeaaaaaaaaaapuq",
      "chat_id": 2002,
      "name": "Alice — @alice",
      "order": "9223372036854775807",
      "pinned": true
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `schema` | Constant `gramdrive.order` — what this file is, for a reader who found it alone |
| `schema_family` | DOM-023 schema lineage; matches the family in the document's `ItemId` |
| `list` | `{"kind":"main"}`, `{"kind":"archive"}`, or `{"kind":"folder","folder_id":<i32>}` |
| `chats[].rank` | 0-based position. **This is the order a reader should use** |
| `chats[].id` | The chat appearance's `ItemId` text — joins the order to the enumerated tree |
| `chats[].chat_id` | Telegram chat ID, a JSON number (int53 by Telegram's schema, so no precision loss) |
| `chats[].name` | The folder name as projected on disk, sanitized and suffixed |
| `chats[].order` | Telegram's raw rank, **a string** — see below |
| `chats[].pinned` | Whether the chat is pinned in this list |

`order` is a string because it is an int64 and a JSON number is an IEEE-754
double to most parsers (JavaScript, `jq`): the top-of-range values Telegram
gives pinned chats round silently, and two distinct pinned chats can compare
equal after the round trip. `chat_id` is int53 by Telegram's own schema, where
no such loss exists, and stays a number. Readers should not need `order` at
all — `rank` is the resolved answer.

Tests: `tests/ordering_fixture.rs` pins the schema and the acceptance
criteria by example — a reorder changing metadata only, a rename changing the
name only, and a chat failing to shadow `order.json`;
`tests/ordering_properties.rs` proves them over sampled input, including
titles built to break a JSON writer, and parses the rendered document with an
independent reader rather than asserting it against a string the writer built.

## Test command

```sh
cargo test -p gramdrive-model
```
