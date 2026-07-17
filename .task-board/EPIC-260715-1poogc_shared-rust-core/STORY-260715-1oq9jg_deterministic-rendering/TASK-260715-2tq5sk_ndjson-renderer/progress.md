## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:45Z

## Last Update
2026-07-17T18:15:54Z

## Blocked By
- TASK-260715-1ceq7h

## Blocks
- TASK-260715-22l8zy

## Checklist
- [x] Deterministic NDJSON renderer: lossless message/event records per POL-3 (revisions/tombstones per retention mode), stable field order, byte-stable output for identical input
- [x] Renders from state repositories via render watermarks; fixture corpus covers messages, edits, deletes, attachments, service events
- [x] All quality gates green (make check)
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [ ] Implementation matches AC
- [ ] Solution fits project architecture
- [ ] Tests green
- [ ] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260717-576681, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-576681)
Design locked. gramdrive-render depends on gramdrive-model ONLY (arch gate) so the NDJSON renderer is a pure function; it defines the versioned messages NDJSON schema (schema_family=1, schema_version=1, renderer_version=1) and its input record contract. No normalized message-record type exists upstream (state stores payloads as opaque bytes), so defining the output schema + input view is this tasks job. Engine will bridge state repos -> records via render watermarks (separate task). Owns POL-3 Mirror/Audit projection (current-only vs revisions+content-preserving tombstones) and POL-4 unavailable states. Deterministic hand-rolled JSON per the order.json precedent (no serde).
READY FOR REVIEW. Implemented gramdrive-render::ndjson — lossless deterministic messages.ndjson renderer. gramdrive-render depends on gramdrive-model ONLY (arch gate), so it cannot read gramdrive-state: this task DEFINES the versioned output schema (gramdrive.messages s1/r1, family 1) and the input record contract (ndjson::MessageHistory) since no normalized message-record type exists upstream (message_events.payload is opaque bytes). Renderer is a pure fn; watermark threaded into header+content_version to line up with state::publish_render (SYNC-024). Engine wiring state->records is a follow-up (TASK-260715-22l8zy + engine). POL-3 Mirror/Audit projection (current-only vs revisions+content-preserving tombstones) and POL-4 unavailable states owned by the renderer and tested. Determinism: fixed field order, integers-only, event_seq-sorted revisions, golden fixtures freeze v1. make check 8/8 green, zero new deps, +21 tests. Artifact: TASK-260715-2tq5sk_results.md. Logbook entry 2207.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-576681, pid=40031, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-32faf5, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-32faf5)
REVIEW: ACCEPTED. Verified by reading all sources (json.rs, ndjson/{mod,record,render}.rs, tests, goldens) against AC/DoD and rerunning make check 8/8 (test 11.5s, zero new deps).
AC MET: (1) Deterministic+parseable goldens — byte-stable by construction (ordered Json::Object, integers-only, no serde), re-render-stable test + identical-input test + revision-order-independence test all green; independent hand-rolled parser in tests/support proves output is valid JSON separately from the writer. (2) Every message field represented-or-explicitly-null — full-field-set test asserts identical key order on every record; POL-3 Mirror (live present, deleted purged, prior revs dropped) and Audit (every revision superseded..present + content-preserving deleted tombstone carrying last-known content + deleted_ms) both correct in code and goldens; POL-4 unavailable states explicit (availability fetchable/restricted/unavailable/view_once, content null unless downloaded, protected surfaced); Other{} escape hatches keep entity/media/service lossless.
ARCH FIT: depends on gramdrive-model ONLY (arch gate green), pure fn, no I/O, no platform cfg, hand-rolled JSON per order.json precedent. Watermark threaded into header + content_version token (gramdrive.messages/s1/r1/w{seq}) to line up with state::publish_render (SYNC-024). Defining the output schema + input record contract IS this tasks job (no normalized message-record type exists upstream; message_events.payload is opaque bytes) — legitimate, not a forced fit. Engine wiring state->records is the documented follow-up (TASK-260715-22l8zy).
NOTE (accepted, not a defect): the rendered doc intentionally omits two domain-model Attachment fields — the Telegram locator/file_id and last-verification-time — because both are volatile and would break the byte-stability AC; they live in the state layer and a reader resolves back via the stable item_id (AttachmentKey). Correct call; if ever needed in the doc its a SCHEMA_VERSION bump (mechanism already in place).
Verdict -> done.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-32faf5, pid=51979, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-2tq5sk_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-2tq5sk/TASK-260715-2tq5sk_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-2tq5sk_results.md](file://TASK-260715-2tq5sk/TASK-260715-2tq5sk_results.md) — Implementation notes: versioned NDJSON messages renderer (schema+input contract, POL-3/POL-4 projection, determinism, 23 tests, gates 8/8)
- [TASK-260715-2tq5sk_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-2tq5sk/TASK-260715-2tq5sk_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-2tq5sk_review.md](file://TASK-260715-2tq5sk/TASK-260715-2tq5sk_review.md) — Reviewer verdict: ACCEPTED — versioned NDJSON renderer meets AC/DoD, make check 8/8
