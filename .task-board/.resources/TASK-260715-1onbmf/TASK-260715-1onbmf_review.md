# Review verdict: ACCEPTED (TASK-260715-1onbmf)

Reviewer: [reviewer] reviewer (claude), 2026-07-18. Read-only review; no code modified.

## Verdict

Accepted -> done. Implementation matches all AC, fits the crate architecture, gates independently re-verified green.

## AC verification

1. **Downloads resume/retry safely** — synchronous downloadFile resumes from TDLib cache prefix; recovery after flood wait (429, stated delay intact), transport failure (500), and stale reference all pinned by tests/file_download.rs; non-covering response classified Unavailable (retryable), never silent bytes.
2. **Cancellation propagates** — drop-at-await + CancelGuard fires cancelDownloadFile{only_if_pending:false}, pinned on the wire (an_abandoned_download_fires_the_network_cancel); sink Stop -> SourceError::Cancelled; self-waking yield gives a cancellation point per delivered slice.
3. **Locator refresh invisible to identity** — FILE_REFERENCE_* -> getMessage -> remote_unique_id verified against pin (drift => VersionConflict, restricted => Restricted, gone => NotFound) -> StaleReference; test asserts exactly [downloadFile, getMessage] on the wire and the retry succeeds. Matches SYNC-045.
4. **Conformance bytes match** — SYNC-002 suite runs whole with zero skips (tests/fetch_conformance.rs); every fetch/failure/cancellation case flows through the real adapter over mock tdjson + real temp files; harness name honestly declares the fake-enumeration split.

## Independent re-verification by reviewer

- make check: 8/8 green (fresh run; provenance .temp/acceptance/local-all).
- make tdjson-smoke: green against staged artifact — the machine-built downloadFile/getMessage/cancelDownloadFile payloads parse in the real TDLib.
- Spec cross-check: SYNC-040..046 (.spec/sync-and-filesystem-semantics.md), POL-4 (.spec/policies.md) — claims hold.
- Signature claim proven by compilation: RangedTdjsonSource::fetch (conformance harness) delegates to TdDownloader::fetch verbatim inside a DriveSource impl.
- POL-4 zero-request pin is real: the restricted-availability test runs with no mock responder, so any network call would panic.
- Mid-fetch version verification cadence pinned exactly (FlippingCatalog: gate + one resolve per slice).
- Architecture: gramdrive-source as product dep and gramdrive-testkit dev-only both allow-listed; architecture gate green; sans-IO machine + thin driver matches the crate convention; FetchCatalog seam keeps metadata ownership in the state layer; temp-file ownership honored (read-only, asserted by untouched-content check).

## Non-blocking nit (recorded, no rework required)

- download.rs LockTable: if every woken waiter is dropped before re-acquiring, an empty LockSlot (held:false) stays in the map until the same file_id is fetched again. Bounded, reused, no correctness impact.

## Cross-finding

- BUG-260718-17hzcx (sanitize idempotence, gramdrive-model) correctly filed as a separate board item with repro; unrelated to this diff — confirmed: naming code untouched here.
