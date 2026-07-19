# TASK-260715-i3mp9x — Map core items to NSFileProviderItem

Status: ready for review (board `to-review`).

## What shipped

A pure, total mapping from the core provider projection (`ItemMetadata` +
`AccountInfo.rootItemId`) to `NSFileProviderItem`, plus the identifier
translation it needs and the `item(for:)` wiring that returns mapped items.

New / changed source (`apple/GramDriveSupport/`):

- `Sources/GramDriveFileProvider/ItemIdentifierMapping.swift` — core `ItemId`
  text ⇄ `NSFileProviderItemIdentifier`, folding the account root onto the
  reserved `.rootContainer` (and a direct child's parent onto it) and passing
  every other id through verbatim (DOM-024).
- `Sources/GramDriveFileProvider/FileProviderItem.swift` —
  `GramDriveFileProviderItem: NSFileProviderItem`. Deterministic map of every
  provider-visible field: identity/parent, `filename` (the collision-free
  `safeName`, SYNC-012), content type, `documentSize`, `itemVersion`,
  timestamps, `capabilities`, `fileSystemFlags`.
- `Sources/GramDriveFileProvider/FileProviderExtension.swift` — `item(for:)`
  now returns the mapped item, via a test-friendly `resolveItem(for:)` seam.
- `Sources/SharedStateSmoke/main.swift` + `.scripts/smoke/run_shared_state_smoke.py`
  — the `domains` mode now maps the seeded tree and asserts the read-only
  surface cross-process (mode `2c`).
- `Tests/GramDriveFileProviderTests/FileProviderItemTests.swift` — the mapping
  suite; `FileProviderExtensionTests.swift` — `resolveItem` error paths.

## Key mapping decisions

- **Read-only surface (DEC-007 / SYNC-060).** Directories advertise
  `.allowsContentEnumerating`; fetchable files `.allowsReading`;
  restricted/unavailable content (POL-4) advertises nothing. No mutating
  capability (write / rename / reparent / trash / delete / add-subitem) is
  ever produced, for any kind — the invariant SYNC-061 depends on.
- **Identity folding.** The account root (`ItemMetadata.parent == nil`)
  becomes `.rootContainer` and is its own parent; a direct child of the root
  reparents onto `.rootContainer`; deeper items keep their durable ids.
- **POL-4 unavailable/protected.** A restricted or unavailable item is a real,
  visible item (keeps its type and size); only its byte access is withheld.
- **POL-3 tombstones / unknown ids.** `item(for:)` answers `noSuchItem` for an
  unknown id or a `deletedAtMs != nil` tombstone; a transient storage failure
  passes through unchanged so the system retries.
- **Versions.** `metadataVersion`/`contentVersion` tokens map to
  `NSFileProviderItemVersion` components. Core caps tokens at 256 bytes but the
  File Provider component limit is 128 — a token over 128 bytes folds to its
  SHA-256 digest (equality-preserving, fixed 32 bytes). An absent content
  version maps to a `0x00` sentinel, which cannot collide with a real token
  (control bytes are forbidden in tokens).
- **Content type.** Prefer a *declared* `UTType` — MIME first, then filename
  extension — before accepting a dynamic type; `.data` last. Necessary because
  `UTType(mimeType:)` synthesizes a `dyn…` type for an unknown MIME rather than
  failing.

## Platform findings (see LOGBOOK 1704)

- macOS aliases `AllowsContentEnumerating == AllowsReading` (bit 0) and
  `AllowsAddingSubItems == AllowsWriting` (bit 1). Read-only ⇒ bit 0 only.
- `UTType(mimeType:)` returns a dynamic type for unknown MIME, not nil.
- Core's 256-byte token limit exceeds the File Provider 128-byte version-
  component limit.

## Verification

- `swift test` (full package): **170/170 passed**, incl. the new item-mapping
  suite (every `ItemKind`, every `ItemAvailability`, no write/delete leak) and
  `resolveItem` error paths.
- `make smoke-shared-state`: **PASSED** — the `domains` mode maps the seeded
  account root / chat dir / `photo.jpg` attachment and asserts the folded
  identifiers, `public.jpeg`/2048-byte file, and `readonly=true` on every item,
  cross-process over metadata a separate Rust coordinator wrote.
- `make check` gate steps touched by this change (Swift+Python only): `scripts`
  self-tests and `traceability` both green. The Rust-only gates
  (toolchain/format/lint/test/architecture/supply-chain) were not re-run — no
  Rust source changed.

## Boundary

Enumerators, working-set, and change anchors are the next task
(TASK-260715-rhcnhc); content fetch is a later story. This task delivers the
item mapping and `item(for:)` only.
