# TASK-260715-265gqq — Review verdict (2026-07-17)

**Verdict: changes requested → to-dev.** One narrow doc-consistency defect; everything substantive is accepted and independently verified. The rework is a three-passage doc fix — do not touch anything else.

## Independently verified (do not redo)

- `make check` re-run by reviewer: **8/8 green** (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts).
- `make smoke-bindings` re-run by reviewer: **SWIFT SMOKE PASSED, KOTLIN SMOKE PASSED** — contract version, constructor + async error round-trips (InvalidArgument), per-chunk progress, token cancellation round-trip as Cancelled with no callbacks afterwards, Kotlin coroutine-cancel bonus. Consumers assert exactly the AC.
- Provider-neutrality: grep over the FFI surface clean — the only Telegram mention is a doc comment saying the probe runs *without* a Telegram account. Zero TDLib/gotd/OS-native types (DEC-003).
- Threading/async model, callback dispatch, cancellation rationale, error contract, versioning policy: documented in crates/gramdrive-ffi/README.md, matches the code.
- **Deviation ENDORSED**: per-crate `[licenses.exceptions]` instead of the unblock note`s blanket `[licenses] allow`. DEC-021 says *named* POL-6 exceptions; exceptions is the mechanism that makes named enforceable and is strictly narrower. The unblock instruction was the imprecise artifact, not the implementation. POL-6 prose correction and the DEC-021 TRACEABILITY row are also correct.

## Defect (the only rework)

Three uncommitted passages still describe the pre-DEC-021 state — that the supply-chain gate is red and the licensing decision is pending. DEC-021 was accepted 2026-07-17 and the gate is green; committing these as-is would land factually wrong compliance status in the same commit that implements the accepted decision. The implementer corrected POL-6 prose for exactly this inconsistency class but missed their own earlier prose:

1. `Cargo.toml` (~lines 30-32): comment `uniffi is MPL-2.0 — outside the POL-6 allow list; the supply-chain gate fails until the owner accepts the pending licensing decision row`.
2. `crates/README.md` (~lines 205-212): paragraph `**Known gap — supply-chain gate is red pending a licensing decision.** ... The pending decision is recorded in docs/OPEN_QUESTIONS.md ...`.
3. `crates/gramdrive-ffi/README.md` (~lines 148-151): `**License gate status**: ... the supply-chain gate stays red until the owner accepts the pending decision row ...`.

Fix: rewrite each to state the current fact — MPL-2.0 (uniffi* family) and Unicode-3.0 (unicode-ident) are owner-accepted named exceptions per DEC-021, enforced as per-crate `[licenses.exceptions]` in deny.toml; gate green. Repo-wide grep for `stays red|is red|pending decision|pending licensing|until the owner accepts` found exactly these three files (LOGBOOK entries are append-only history — leave them). After the fix re-run `make check`; smoke re-run not required for a doc-only change.