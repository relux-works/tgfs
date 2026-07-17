# TASK-260715-1jmsdp — Ordering projection modes

**Status:** ready for review. All quality gates green (`make check` 8/8).

## What shipped

POL-1 / DEC-013 ordering projection in `gramdrive-model` (layer 0). Folder names
stay stable; Telegram's exact dialog order is published as data at each chat-list
root. Four pieces, all additive:

| Piece | File | Why it was needed |
|---|---|---|
| `ordering` module | `src/ordering.rs` (new) | Position snapshots -> deterministic `order.json` |
| `OrderDocKey` + tag `0x0b` | `src/identity.rs`, `src/identity/codec.rs` | `GeneratedDocKey` is chat-scoped and cannot name a *list*-scoped document |
| `SiblingName::fixed` | `src/naming.rs` | A chat titled `order.json` must not shadow the metadata |
| `order.json` tree node | `src/tree.rs` | POL-1 puts the file at each list root, so it must be an enumerable node |

## Decisions worth reviewing

1. **Numeric-prefix mode is absent, not disabled** (DEC-013, checklist item 1).
   No mode enum, no dead renderer, no migration path. A mode shipped switched off
   is untested code that reads as a supported feature. Revisiting post-v1 means a
   new decision row and a new projection mode beside this one.

2. **`order` is a JSON string, `chat_id` is a JSON number.** `chatPosition.order`
   is int64 and a JSON number is an IEEE-754 double to most parsers (JavaScript,
   `jq`): the top-of-range values Telegram gives pinned chats round silently, and
   two distinct pinned chats can compare equal after the round trip. `chat_id` is
   int53 by Telegram's own schema, so it stays a number. Readers should use
   `rank`, which is the resolved answer.

3. **`is_pinned` is recorded but is not a sort key.** Telegram already encodes
   pinning in `order`; sorting by it again would be a second, disagreeing
   implementation of the server's ranking. It is kept because the pinned boundary
   is not recoverable from the sequence alone and the app UI draws it.

4. **Fixed names are the one asymmetry in `resolve_siblings`.** That function is
   deliberately symmetric (every collision member is suffixed). A constant is the
   principled exception: it privileges nothing arbitrary and cannot be deleted, so
   neither objection behind the symmetry applies. See the rewritten rustdoc.

5. **JSON writer is hand-rolled**, following the precedent of the identity codec's
   base32 (POL-6: a dependency is supply-chain surface). Rust `&str` is valid
   UTF-8, so the lone-surrogate case cannot arise. The property suite parses the
   output with an *independent* mini reader rather than asserting it against a
   string the writer built.

## Bug found and fixed during self-review

The first implementation detected duplicate chats by comparing sorted neighbours.
That was wrong: the sort key starts with `order`, not `chat_id`, so two records
for chat 5 (orders 20 and 10) are separated by any chat whose order falls between
them. The duplicate would have reached `resolve_siblings`, violating its
distinct-id precondition and producing two identically-named folders plus a chat
listed twice in `order.json`. Replaced with a set-based scan after the sort
(deterministic error regardless of input order).

Both regression tests were confirmed to fail against the reverted fix:
`duplicate_chat_records_are_rejected_even_when_not_adjacent` (fixture) and
`duplicate_chats_are_rejected_at_any_distance` (property). The proptest seed is
checked in at `tests/ordering_properties.proptest-regressions`, matching the
existing `naming_properties` convention.

## Tests

118 tests pass in `gramdrive-model` (was ~97).

- `tests/ordering_fixture.rs` (13) — AC by example: reorder changes metadata only
  (ids/names/paths byte-identical); rename changes the name only; chat cannot
  shadow `order.json`; schema pinned; each list kind; duplicate rejection.
- `tests/ordering_properties.rs` (8) — AC over sampled input: rendering
  independent of record order; ranks dense and correctly sorted; positions never
  reach identity or names; names unique and never shadow the document; document is
  well-formed JSON for hostile titles; duplicates rejected at any distance.
- `tests/naming_collisions.rs` (+4) — fixed names never suffixed, win against case
  variants, cost nothing when unrelated, and resolve alongside colliding titles.
- `tests/tree_fixture.rs` (+5) — `order.json` at every list root and *not* under
  the folder catalog (which is not a list); resolves canonically; read-only; a
  foreign schema family or unknown folder is not a node; it is not a directory.
- `tests/identity_golden.rs` (+2) — tag `0x0b` encodings pinned (`order_doc_main`,
  `order_doc_folder`). Pre-existing goldens unchanged, which pins that the
  addition is purely additive.
- `tests/identity_properties.rs` — `arb_canonical` now generates `OrderDoc`, so
  round-trip and cross-kind injectivity cover it.

## Verification

```
make check   # 8/8: toolchain, format, lint, test, architecture, supply-chain,
             #      traceability, scripts
```

## Flagged for review (not changed here)

`.spec/sync-and-filesystem-semantics.md` SYNC-011 still reads "Numeric order
prefixes are an optional presentation mode", which predates DEC-013 ("no numeric
prefixes in v1", accepted 2026-07-17). The traceability matrix already reads
SYNC-011 as "Stable-name mode with order metadata per POL-1", and this task's
checklist requires the prefix mode to be absent — so the code follows DEC-013.
The stale spec sentence is left for the owner rather than rewritten unilaterally,
since specs are owner-governed. Suggested wording: state that stable-name mode is
the v1 projection and numeric prefixes are deferred post-v1 per DEC-013/POL-1.
