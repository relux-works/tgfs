# gramdrive-model

Domain vocabulary of the GramDrive core: item identity, the virtual
`chat -> folder -> files` tree, naming/sanitization policy, versions, change
cursors, and byte ranges. Layer 0 — every other crate depends on it; it
depends on nothing inside the workspace.

## Ownership

STORY-260715-3qxar5 (identity-and-namespace), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1qz1g5 (stable item identities),
TASK-260715-3tjduq (virtual tree builder), TASK-260715-1ffbkg (cross-platform
naming), TASK-260715-1jmsdp (ordering projection).

## Dependencies

Internal: none. Platform-specific code: forbidden. See `crates/README.md`.
Dev-only: `proptest` (identity property suite); it must never move to
`[dependencies]` — test-support code does not ship (crates/README.md).

## Stable item identities (DEC-008, DOM-001..DOM-024)

The `identity` module owns the typed keys and their opaque serialization —
TASK-260715-1qz1g5. The model:

- **Canonical keys** (`CanonicalKey`) name source-derived records: account,
  chat list (Main/Archive/folder), chat, message, attachment, generated
  NDJSON/Markdown document, and content-addressed blob. Telegram-derived keys
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
`0x07` (account id: i64, hash), appearance `0x10` (list kind, then the
wrapped canonical key's tag and fields). Scope = account id (i64) +
namespace version (u32). List kind: Main `0x01`, Archive `0x02`, Folder
`0x03` + folder id (i32). Partition: chat `0x01`, year `0x02` + u16, month
`0x03` + u16 + u8. Format: NDJSON `0x01`, Markdown `0x02`. Hash: SHA-256
`0x01` + 32 digest bytes.

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

## Test command

```sh
cargo test -p gramdrive-model
```
