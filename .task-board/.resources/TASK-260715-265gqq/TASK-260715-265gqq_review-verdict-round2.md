# TASK-260715-265gqq — Review verdict, round 2 (2026-07-17)

**Verdict: ACCEPTED → done.** The three stale doc passages are fixed, scope was
respected exactly, and every AC re-verified independently by this reviewer.

## The round-2 defect is fixed

All three passages named in `TASK-260715-265gqq_review-verdict.md` now state the
current fact — DEC-021 accepted, per-crate `[licenses.exceptions]`, gate green:

1. `Cargo.toml` (comment above the `uniffi` dep) — states the named exception and
   that the grant reaches only the `uniffi*` crates named in deny.toml.
2. `crates/README.md` — "Known gap ... gate is red" became "Licensing — two named
   POL-6 exceptions, gate green", covering both MPL-2.0 and Unicode-3.0, and drops
   the stale `docs/OPEN_QUESTIONS.md` pending-decision pointer.
3. `crates/gramdrive-ffi/README.md` ("License gate status") — named exception per
   DEC-021 with the `.spec/decisions.md` pointer; gate green.

Crucially the fix states the *right mechanism*: all four descriptions (policies.md
POL-6, deny.toml, crates/README.md, ffi/README.md) now agree that the grant is
named and per-crate, not a blanket allow. A doc fix asserting the wrong mechanism
would have been a new defect; it does not.

## Independently verified by reviewer (not inherited from round 1)

- **`make check` re-run: 8/8 green, exit 0** (toolchain, format, lint, test,
  architecture, supply-chain, traceability, scripts). Exit code checked directly.
- **`make smoke-bindings` re-run: exit 0** — SWIFT SMOKE PASSED, KOTLIN SMOKE
  PASSED, BINDINGS SMOKE PASSED. Cancellation interrupts the probe early and no
  progress callbacks arrive after cancellation, in both languages.
- **Doc claims cross-checked against reality**: deny.toml `exceptions` entries are
  per-crate and unversioned as documented; DEC-021 row in `.spec/decisions.md` is
  Accepted and matches; POL-6 prose in `.spec/policies.md:59` agrees.
- **Stale-phrase grep** (`stays red|is red|gate fails|pending decision|pending
  licensing|until the owner accepts`) over `*.md`/`*.toml`: zero hits in source or
  docs. Remaining hits are only board artifacts and LOGBOOK — append-only history
  that correctly describes the past state.
- **Provider-neutrality (DEC-003)**: grep over `crates/gramdrive-ffi/src/` and
  `uniffi.toml` for Telegram/TDLib/gotd/mtproto/NSString/jstring/jni/CFString/objc
  returns three hits, all doc comments *asserting* neutrality (one notes the probe
  runs without a Telegram account). Zero provider or OS-native types. FFI deps are
  `gramdrive-engine`, `gramdrive-model`, `uniffi`, `tokio` only.
- **Threading/async + versioning DoD**: `crates/gramdrive-ffi/README.md` documents
  the poll protocol and who drives futures, tokio as the core runtime, the
  no-blocking rule (NFR-025), callback dispatch (background thread, never a
  platform main thread, host must hop for UI), panic→binding-error behavior,
  cancellation rationale, error contract (NFR-030), and the versioning policy
  (`CONTRACT_VERSION`, checksum-pinned bindings, toolchain-pinned uniffi).

## Scope discipline

The unblock note said "touch nothing else". Verified by mtime: exactly three files
were modified in round 2 (`Cargo.toml` 05:12:10, `crates/README.md` 05:12:17,
`crates/gramdrive-ffi/README.md` 05:12:22). Every other uncommitted file predates
the 05:11 unblock note and is round-1's already-accepted work, unaltered.

## Note for the committer

The working tree still holds all of round-1's uncommitted work — the `git diff`
is far larger than three hunks by design. Nothing here is staged or committed.

## Quality note

Worth keeping: the Swift cancellation limitation was *measured* against uniffi
0.32 generated code (the `CALL_CANCELLED` handler is a `fatalError` placeholder),
not assumed, and that measurement is what justifies the explicit-token contract
over binding-runtime cancellation. The token also maps 1:1 onto `NSProgress`
cancellation handlers and Android `CancellationSignal`. That is a real constraint
driving a real design decision, with a re-evaluation trigger on uniffi upgrade.
