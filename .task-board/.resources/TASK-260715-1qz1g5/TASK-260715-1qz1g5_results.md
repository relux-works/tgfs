# TASK-260715-1qz1g5 — Stable item identities: implementation notes

## What landed

New `identity` module in `gramdrive-model` (layer 0), per DEC-008 and DOM-001..DOM-024:

- `crates/gramdrive-model/src/identity.rs` — public typed keys + `ItemId` + `IdParseError`
- `crates/gramdrive-model/src/identity/codec.rs` — private v1 binary codec + strict base32 text codec
- `crates/gramdrive-model/tests/identity_properties.rs` — 13 proptest properties
- `crates/gramdrive-model/tests/identity_golden.rs` — pinned v1 encodings + parser error paths
- `crates/gramdrive-model/README.md` — format spec, version policy, collision behavior (the AC's "collision behavior documented")

## Type model

**Canonical keys** (`CanonicalKey`): `Account`, `ChatList` (Main/Archive/`FolderId`),
`Chat`, `Message`, `Attachment`, `GeneratedDoc` (chat + partition + format + schema
family, DOM-023), `Blob` (account + `ContentHash::Sha256`).

**Appearance keys** (`AppearanceKey`): `{ view: ChatListKind, item: CanonicalKey }` —
one chat visible in Main/Archive/folders is N appearances over one canonical key
(DOM-002/DOM-022/PRD-013). Structurally non-nesting; view carries no account, so a
view/item account mismatch is unrepresentable.

**Scoping (DOM-021):** Telegram-derived keys embed `AccountScope` = `AccountKey` +
`NamespaceVersion` (epoch that retires an account's derived namespace at once, e.g.
re-login as a different user). `AccountKey` itself and `BlobKey` deliberately exclude
the epoch — the account item survives a bump; content identity is orthogonal to it.

No key type carries a string or ordering position → no path/title/order dependence
*by construction* (DOM-001/DOM-005), on top of the property proofs.

## Serialization (`ItemId`, DOM-020/DOM-024)

- Binary: `version(0x01) | kind tag | fixed-width BE fields` — a prefix code,
  injective by layout. Max v1 key = 40 bytes (attachment appearance).
- Text: `"gd"` + unpadded lowercase RFC 4648 base32, strict canonical parse
  (lowercase only, no padding, zero trailing bits → exactly one spelling per key).
- Both `parse_bytes`/`parse_text` fully validate; `ItemId::key()` is infallible.
- Hand-rolled codec, no serde: determinism/injectivity provable from the layout,
  zero added product-dependency surface.
- Unknown version → `UnsupportedVersion` (future formats coexist); unknown tags name
  the field; truncation and trailing bytes are distinct errors.

## Proof obligations → tests

| AC | Test |
|---|---|
| Determinism | `encoding_is_deterministic`, golden pins |
| Round-trip | `round_trips_through_bytes` / `_text` (also the collision-freedom proof: decode is a function) |
| Namespace separation | `distinct_keys_never_collide` (full injectivity sample), `canonical_and_appearance_namespaces_are_separate`, `views_separate_appearances_without_touching_canonical_identity`, `namespace_version_scopes_derived_identities` |
| Version compatibility | golden fixtures pin v1 byte-for-byte; `foreign_version_bytes_are_rejected` |
| No path/title dependence | structural (no string/order fields exist); documented in module + README |
| Parser strictness | `proper_prefixes_never_parse`, `trailing_bytes_never_parse`, `uppercased_text_never_parses`, base32 canonicality units |

## Supply chain

`proptest` added as **dev-dependency only** (`default-features = false, features =
["std"]` — drops fork/timeout machinery). `deny.toml` `allow-build-scripts` gained
three documented name-level entries: `num-traits` (autocfg probe), `zerocopy`
(cfg emitter via ppv-lite86/rand_chacha), `wit-bindgen` (getrandom WASI-target dep,
never compiled for a GramDrive target). Licenses/advisories/sources gates unchanged.

## Verification

- `cargo test -p gramdrive-model`: 31 tests green
- `make check` (suite `all`): 8/8 green — toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts
- Provenance: `.temp/acceptance/local-all/`

## Notes for dependents

- Virtual tree builder (TASK-260715-3tjduq): directory kinds (year/month/media_dir)
  extend the canonical tag space (0x08.. free; 0x10 = appearance). Which
  (view, item) combinations exist is tree-builder discipline.
- Durable inode mapping (TASK-260715-1za16i): map inodes ↔ `ItemId` binary form.
- SQLite schema (TASK-260715-1ceq7h): store `ItemId::as_bytes()` (BLOB) or text form.
- `DocPartition`/`SchemaFamily` values are not semantically validated at this layer;
  renderer (STORY-260715-1oq9jg) assigns family numbers.
