# TASK-260715-1qz1g5 — Review verdict: ACCEPTED (done)

Reviewer: reviewer (claude), 2026-07-17. Read-only review; no code modified.

## What was reviewed

- `crates/gramdrive-model/src/identity.rs` — public typed keys, `ItemId`, `IdParseError`
- `crates/gramdrive-model/src/identity/codec.rs` — v1 binary codec + strict base32 text codec
- `crates/gramdrive-model/tests/identity_properties.rs` — 13 proptest properties
- `crates/gramdrive-model/tests/identity_golden.rs` — pinned v1 encodings + error paths
- `crates/gramdrive-model/README.md` — format spec, version policy, collision behavior
- Config surface: workspace `Cargo.toml` (proptest dev-dep), `deny.toml` (3 documented
  build-script allowlist entries), crate `Cargo.toml`, `lib.rs`, `LOGBOOK.md`

## AC verification

| AC | Verdict | Evidence |
|---|---|---|
| Typed keys for account, chat list/folder, chat, message, attachment, generated doc, blob (DEC-008) | PASS | `CanonicalKey` covers all seven kinds; fields match DOM-021/DOM-023 exactly (checked against `.spec/domain-model.md`) |
| Appearance keys separate from canonical (one chat in Main/Archive/folders) | PASS | `AppearanceKey` = view × `CanonicalKey`, structurally non-nesting; nested appearance also unparseable (inner tag position rejects `0x10`); properties `canonical_and_appearance_namespaces_are_separate`, `views_separate_appearances_without_touching_canonical_identity` |
| Opaque, versioned serialization stable across restarts/updates | PASS | Version byte + fixed-width prefix code; golden fixtures pin v1 byte-for-byte; `foreign_version_bytes_are_rejected` gates coexistence of future formats |
| No path/title/order dependence | PASS | Structural — no key type carries a string or ordering position, so no input exists through which a rename could reach an encoding. Stronger than a sampled property. `AttachmentIndex` is an assign-once normalization ordinal per DOM-021, not display order |
| Round-trip + namespace-separation property tests green | PASS | Re-ran independently: 13/13 property, 8/8 golden, 10/10 unit — 31/31 green |
| Collision behavior documented | PASS | README "Collision behavior" section: encoding injectivity proven by round-trip; residual surface (SHA-256 inputs, Telegram ID reuse → `NamespaceVersion`, display names → naming policy) correctly attributed |
| All quality gates green | PASS | Re-ran `make check`: 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts) |

## Architecture fit

- Layer 0 (`gramdrive-model`), zero internal deps, no product deps added — proptest is
  dev-only with `default-features = false`. Architecture gate green.
- Hand-rolled codec instead of serde is the right call at this layer: injectivity is
  provable from the layout, and the format-freeze contract lives in golden tests, not
  in a third-party crate's behavior.
- deny.toml build-script allowlist entries are name-level, documented, dev-tree-only.
  Supply-chain gate green.
- Traceability rows (DEC-008, DOM-020/021/023/024 → this task) consistent.

## Findings (non-blocking)

1. **Wrong max-size claim in comments/notes.** `codec.rs:64` says "Largest v1 key
   (attachment appearance) is 40 bytes"; the actual v1 maximum is the **blob
   appearance at 49 bytes** (1 ver + 1 tag + 5 folder view + 1 blob tag + 8 account +
   1 hash tag + 32 digest). Canonical blob alone is 43. The same figure is repeated in
   the results artifact. Impact: none on behavior — the enforceable bound is the
   `encoded_size_is_bounded` test (≤64 bytes / ≤128 text chars), and
   `Vec::with_capacity(48)` just reallocs once for blob appearances. Fix the comment
   opportunistically in a later touch of this file; dependents must size from the
   tested ≤64 bound. Recorded in LOGBOOK 2026-07-17 0655.

## Verdict

Accepted → `done`. Implementation matches AC, fits the architecture, all gates and
tests independently re-verified green.
