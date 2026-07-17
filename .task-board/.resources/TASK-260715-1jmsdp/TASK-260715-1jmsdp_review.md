# Review — TASK-260715-1jmsdp: Implement ordering projection modes

Verdict: **ACCEPTED** → `done`
Reviewer: reviewer (claude), 2026-07-17
Basis: working tree at b8d9b2b + uncommitted changes (nothing staged or committed).

## Gates — independently re-run

- `make check`: **8/8 green** (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts).
- `cargo test -p gramdrive-model`: **118 passed, 0 failed** across 10 binaries; `ordering_fixture` 13/13, `ordering_properties` 8/8.
- Nothing staged, nothing committed — the "never commit automatically" constraint was respected.

## AC verification

AC: *"Reorder fixtures produce expected metadata/path changes while stable IDs and cached content remain intact."*

Met, and pinned by executable tests rather than asserted in prose:

- `reorder_changes_metadata_only` — order.json bytes change; the (id, name) set, `doc_id()` and `doc_key()` are byte-identical. This is the AC verbatim.
- `rename_changes_the_name_and_nothing_else` — a title change is the only input that moves a folder; ids untouched (DOM-005).
- `positions_never_reach_identity_or_names` (property) — generalizes both over sampled input.
- Cached content: nothing is keyed by order, so invalidation is structurally impossible rather than merely avoided. Identity stability is the proof.

Scope line mentions "migration between modes". Superseded by DEC-013 (no numeric-prefix mode in v1), and correctly resolved: the mode is **absent**, not present-and-disabled. No dead mode code. This matches the checklist DoD, `policies.md` POL-1, and `docs/TRACEABILITY.md:100`. Following the decision over the stale scope line is the right call.

## Architecture fit

Three additive extensions were required and each is justified rather than convenient:

1. **`OrderDocKey` + canonical tag `0x0b`.** `GeneratedDocKey` is chat-scoped by field type (`chat: ChatKey`), so a list-scoped document is genuinely not representable — this is a real gap, not a preference. Additive in the extension room the v1 range reserved (same move `0x08`–`0x0a` made); pre-existing goldens unchanged, which is what pins additivity. New goldens cover Main and Folder + max schema family.
2. **`SiblingName::fixed`.** The list root is the *only* place in the layout where a GramDrive constant shares a directory with user-controlled titles (verified: chat dirs, year dirs and the folder catalog mix no constants with titles). The asymmetry is reasoned, not special-cased — both rationales for symmetric suffixing (don't privilege an arbitrary member; don't rename a survivor on deletion) provably fail to apply to a constant. Loop termination survives the change: filtering fixed indices can only shrink the colliding set.
3. **`order.json` as a tree node.** Fixed child before derived ones, foreign schema family and unknown folder both correctly resolve to "not a node".

`ordering.rs` is the only `src` consumer of `resolve_siblings`; the tree still defers naming to the consumer, consistent with the existing boundary.

## Self-reported bug fix — independently verified

The duplicate-detection fix is real. Old check was `windows(2)` after sorting, reasoning "sorted ⟹ adjacent". The sort is `(order, chat_id)` **descending**, so `order` is primary: `[(c5,20), (c9,15), (c5,10)]` sorts with the duplicates separated. Confirmed by inspection that the old check misses it and the new set-based scan catches it. Consequence was correctly diagnosed as worse than a doubled entry — the duplicate violates `resolve_siblings`' distinct-ids precondition and yields **two identically-named folders**.

Both regression tests are genuine (the 2-record fixture passed against the bug — the test had agreed with the bug). Proptest seed checked in, following the existing `naming_properties` convention, and the file is not gitignored.

## Quality notes worth recording

- `order` as a JSON string is correct and non-obvious: int64 through an IEEE-754 double silently rounds exactly the top-of-range values Telegram assigns pinned chats. TDLib's own JSON interface does the same.
- The property suite parses the rendered document with an **independent** mini reader instead of asserting against a string the writer built. That is the difference between testing and self-agreement.
- `is_pinned` recorded but not sorted by — avoids a second, disagreeing implementation of the server's ranking.

## Non-blocking follow-ups (do not gate this task)

1. **[owner decision] `.spec/sync-and-filesystem-semantics.md:32` SYNC-011 is stale.** Still reads "Numeric order prefixes are an optional presentation mode", predating DEC-013. Code, `policies.md` POL-1 and `docs/TRACEABILITY.md:100` all read it the DEC-013 way; the spec sentence is the lone holdout. The implementer's decision to flag rather than unilaterally rewrite an owner-governed spec is correct process. Needs a one-sentence owner edit, not rework — accepted independently of it.
2. **[hardening] No cross-module test pins ordering ↔ tree consistency.** `OrderEntry.id` and the tree's ChatList children construct the same `AppearanceKey` independently, and order.json's `name` field must match what a tree consumer derives via `resolve_siblings` (with the doc marked `fixed`). Both hold **by construction today**, and there is no production tree-naming consumer yet, so nothing is broken. But nothing pins them: a future change to appearance construction or a consumer that forgets `fixed: true` would make order.json name paths that do not exist, silently. Cheap test, worth adding when the provider layer lands.

## Conclusion

AC met, architecture fit is good, gates green, tests genuinely test. The self-review caught a bug that the fixtures alone would have shipped, and the documentation explains *why* rather than *what*. Accepted.
