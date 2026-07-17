# TASK-260715-3tjduq — Review: virtual tree builder

Verdict: **accepted → done**. Reviewed working tree at 40a9858 + uncommitted
changes; verification re-run independently.

## Verification (independent)

- `cargo test -p gramdrive-model` — 46/46 green (10 unit, 8 golden,
  13 identity property, 12 tree fixture, 3 tree property).
- `make check` (suite all) — 8/8 green (toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts).

## Acceptance criteria

| AC | Verdict | Evidence |
|---|---|---|
| Fixture tree matches specification | Pass | `tree_fixture.rs::fixture_tree_matches_spec_layout` pins the `.spec/sync-and-filesystem-semantics.md` layout example literally (Account/Main/Chat/chat.json, messages.ndjson, YYYY/MM.md, YYYY/media/, Archive, Telegram Folders); child order chat.json → messages.ndjson → years matches the spec listing |
| Canonical records/blobs not duplicated | Pass | Structural: one `ChatState` per chat in `TreeProjection::chats`; views (`members`) hold only `i64` chat-ID references. Observable: `multiple_appearances_share_canonical_records_and_blobs` — distinct appearance ids, equal `canonical` keys, mirrored subtrees, one shared `BlobKey` |
| Deterministic under shuffled input | Pass | All state BTree-keyed; `output_is_deterministic_under_shuffled_input` shuffles folders, chats, memberships, months, attachments with independent seeds → identical full walks |
| Lazy children, page boundaries | Pass | Construction is O(records); `children()` mints only the requested page. Boundary = last child's `ItemId`, exact-match, snapshot-scoped; foreign cursor → `ForeignPageBoundary`, satisfying SYNC-003 (no silent gap/repeat). `any_page_size_enumerates_exactly_once`, `page_boundaries_chain_without_gaps_or_repeats` |
| Capability metadata read-only (DEC-007/SYNC-060) | Pass | Both `Capabilities` constructors pin all write-side fields `false`; `capabilities_are_read_only_and_links_resolve` walks every node |
| POL-1 stable names | Pass | `chat_display_name` = `<Display Name> — @<username>` (em dash) per policies.md POL-1; `rename_preserves_every_identity` proves SYNC-026 (rename changes zero identities) |

## Architecture fit

- Pure projection in the model crate, no I/O — fits DEC-009 (structured
  records canonical, deterministic views) and the crate-architecture gate.
- Identity format v1 extended additively in the codec's explicitly reserved
  room (tags `0x08`–`0x0a`, format `0x03`); original golden fixtures pass
  byte-identical, new goldens pin the new encodings, property generators
  sample the new variants. No version bump needed — additions only.
- (view, item) discipline (appearances only for chats and their subtrees)
  is documented in module docs, README, and logbook, and enforced by
  `resolve_appearance`. Scope-derived keys make cross-account records
  unrepresentable — better than validation.
- Deferred work is correctly routed, not dropped: sanitization/collision
  suffixing → TASK-260715-1ffbkg, order metadata → TASK-260715-1jmsdp
  (both explicit in docs).

## Findings

1. **Logbook entry mangled (fixed during review).** The implementer's
   LOGBOOK.md edit replaced the `### 0655` header line instead of inserting
   above it, merging the identity-review record (TASK-260715-1qz1g5) into
   the new 0710 entry — its "STATUS: review accepted → done" line then
   misread as this task's verdict. Violates the logbook append-only rule.
   Restored the 0655 header and recorded the regression in the 0714 entry.
   Not a code defect; not worth a rework cycle.
2. **Minor, no action:** `children()` is O(siblings) per call — it
   materializes the full sibling key list and finds the boundary by linear
   scan. Fine for the model layer at v1 scale; the exact-match `ItemId`
   boundary contract permits a seek-based implementation later without any
   API change.
3. `take(limit)` precedes `filter_map(resolve)` in `children()` — a
   non-resolving generated key would silently shrink a page. Verified
   unreachable: every key `child_keys` generates resolves by construction
   (checked case by case; also covered by the `ids_are_unique_and_resolve_back`
   property). Noting for future maintainers only.

## Handoff

Working tree left uncommitted, as received. Files: `src/tree.rs` (new),
`src/lib.rs`, `src/identity.rs`, `src/identity/codec.rs`,
`tests/tree_fixture.rs` (new), `tests/tree_properties.rs` (new),
`tests/identity_golden.rs`, `tests/identity_properties.rs`, `README.md`,
`LOGBOOK.md` (+ review repair of the 0655 header).
