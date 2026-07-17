# Review verdict: ACCEPTED (TASK-260715-2ulon7 -> done)

Reviewer: read-only review; all gates and the real-linkage smoke re-run independently, not trusted from the results doc.

## AC verification

- Concurrency/lifecycle tests under cancellation and shutdown: PASS. 33 deterministic mock tests re-run green (12 unit slot/queue/envelope, 8 lifecycle, 5 cancellation, 4 shutdown incl. the backpressure-deadlock watchdog, 4 update dispatch). Synchronization is order-based (probe round-trips through the in-order receive loop), no sleeps; GUARD timeouts only convert hangs into failures.
- No returned C pointer outlives validity: PASS by construction. The TdSendApi/TdReceiveApi seam traffics only in owned Strings; real.rs copies every tdjson-returned C string before returning (td_execute validity is per-thread in TDLib, noted correctly); request CStrings live across the call. Single receiver enforced twice: receive(&mut self) + once-per-process atomic claim. The AC alternative (careful ownership justification instead of miri/asan) is satisfied by the real.rs module docs; miri genuinely cannot execute FFI.

## Gates (re-run by reviewer)

- make check: 8/8 ok (toolchain, format, lint --all-features -D warnings, test --all-features, architecture, supply-chain, traceability, scripts). Runs artifact-free: the mock-only build is what the gates see.
- cargo test -p gramdrive-source-tdjson: 33 passed.
- make tdjson-smoke: 1 passed in 0.5s against the staged libtdjson.dylib (execute round-trip, correlated getOption version asserted against the minted request id, clean close via authorizationStateClosed, shutdown, single-owner claim).

## Architecture fit

- Correct layer-1 placement under the reserved name; policy rows added in the same change to check_crate_architecture.py and crates/README.md (keep-in-sync rule honored).
- The env gate (GRAMDRIVE_TDLIB_ARTIFACT_DIR -> cfg(real_tdjson)) instead of a cargo feature is the right call given the --all-features gate policy; documented in build.rs, crate README, and the workspace feature policy.
- DEC-003 respected: TdError stays inside this crate; SourceError normalization deferred to the adapter tasks. No provider types leak.
- Lock discipline is sound: the state lock is never held across queue/slot operations; slot wakers fire after lock release; shutdown closes queues before joining the loop (the deadlock fix), proven by the watchdog test.

## Minor observations (non-blocking, no rework requested)

1. Closed clients stay in the clients map for the runtime lifetime (Arc + bool each). Deliberate-looking: preserves ClientClosed for late requests instead of a misleading not-registered Protocol error. Fine for the v1 single-account shape; revisit only if client churn ever matters.
2. real_tdjson_smoke asserts the close request resolves Ok, which relies on TDLib answering ok before authorizationStateClosed arrives. Observed stable (green in both the implementer run and this review), and if the ordering ever flipped, close_client would fail that pending with ClientClosed — a semantically documented outcome. Watch for it if the smoke ever flakes.
3. create_client mints a tdjson client id before the shutdown check, so a shutdown race can discard a freshly minted id. Harmless: TDLib starts a client thread on first request, not on id creation.

## Verdict

Implementation matches AC, fits the architecture, tests green everywhere including real linkage. Accepted; status -> done.