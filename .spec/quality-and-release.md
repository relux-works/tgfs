# Quality and Release Specification

Status: planning baseline
Last updated: 2026-07-15

## Test architecture

- **NFR-001 (V1):** Shared Rust domain/sync/cache code has deterministic unit and property tests.
- **NFR-002 (V1):** Every `DriveSource` implementation passes one conformance suite covering paging, cursors, versions, range reads, cancellation, retries, and source failures.
- **NFR-003 (V1):** Every platform adapter passes a common fixture suite plus native integration tests in supported OS environments.
- **NFR-004 (V1):** Crash/restart tests interrupt enumeration, cursor persistence, rendering, hydration, promotion, eviction, migration, and logout at controlled checkpoints.
- **NFR-005 (V1):** Test fixtures contain synthetic data only and cover Unicode, very large histories, duplicate names, albums/topics, edits/deletes, and inaccessible/protected content.

## Correctness and durability

- **NFR-010 (V1):** Replaying the same source snapshot is idempotent: no duplicate canonical records, virtual items, or blobs.
- **NFR-011 (V1):** Unchanged structured input and renderer versions produce byte-identical generated documents.
- **NFR-012 (V1):** Partial or stale content is never published under a current valid version.
- **NFR-013 (V1):** Database/schema upgrades are transactional or resumable and have downgrade/rollback expectations documented per release.
- **NFR-014 (V1):** The product never sends write/delete operations to Telegram in V1.

## Performance budgets

Budgets are provisional until platform spikes establish baselines; changes require measured evidence.

- **NFR-020 (V1):** Cached metadata enumeration should return its first page within 200 ms at p95 on reference hardware, excluding OS UI overhead.
- **NFR-021 (V1):** Enumeration memory is bounded by page/working-set size rather than total account size.
- **NFR-022 (V1):** Hydration streams to disk and does not hold whole files in memory.
- **NFR-023 (V1):** iOS File Provider stays materially below the verified 20 MB process limit; initial engineering target is at most 15 MB measured high-water usage on supported reference devices.
- **NFR-024 (V1):** Desktop/Android memory, startup, database size, and idle network budgets are measured during the TDLib spike before release targets are finalized.
- **NFR-025 (V1):** Provider callbacks honor platform deadlines and cancellation; no unbounded wait on Telegram/network operations.

## Reliability and observability

- **NFR-030 (V1):** State machines use structured, stable error categories and actionable user states.
- **NFR-031 (V1):** Transfer and sync progress survive process restart where the source supports resume.
- **NFR-032 (V1):** Health data includes last successful source update, cursor, pending transfer count, cache pressure, provider registration state, and redacted recent failures.
- **NFR-033 (V1):** Retry loops are bounded and observable; flood waits never become tight retry loops.
- **NFR-034 (V1):** A repair/reconciliation operation exists and is covered by corruption/missing-file fixtures.

## Compatibility

- **NFR-040 (V1):** Minimum supported OS versions are selected and recorded before implementation starts for each adapter.
- **NFR-041 (V1):** SQLite, serialized metadata, generated export schema, and provider item identity have explicit versioning/migration tests.
- **NFR-042 (V1):** Source and core contracts tolerate additive fields and reject incompatible major versions clearly.
- **NFR-043 (V1):** Cross-platform naming behavior is fixture-driven and stable across releases.

## CI and supply chain

- **NFR-050 (V1):** CI runs formatting, linting, unit/conformance tests, dependency/license checks, and secret scanning for every change.
- **NFR-051 (V1):** Native integration tests run on appropriate signed/test environments before platform release.
- **NFR-052 (V1):** Release artifacts are reproducibly attributable to a commit, signed/notarized as required, and accompanied by dependency/SBOM information.
- **NFR-053 (V1):** No release is built with sample Telegram API credentials or developer test sessions.

## Spike exit gates

### Shared core and TDLib

- Fake-source conformance suite exists.
- tdjson source passes identity, pagination, changes, and ranged-download scenarios.
- Rust/TDLib packaging works for each target architecture selected for the next platform.
- Memory/startup/idle measurements are recorded.

### macOS File Provider

- Finder shows stable placeholders after restart.
- Opening a dataless file hydrates correct bytes and cancellation is safe.
- Title/order changes do not break stable identity.
- Provider/app process restart and account removal are clean.

### Windows CfAPI

- Explorer sync root and placeholder identity survive restart/upgrade.
- Range hydration, cancellation, pin state, and read-only failures behave correctly.
- Provider disconnect/reconnect and partial-transfer recovery pass.

### iOS decision gate

- Extension high-water memory is measured below the target budget.
- Cold hydration behavior is explicitly selected, implemented, and disclosed.
- Files app enumeration and already-materialized opens work without the containing app.

## Release gate

A platform release requires reviewed product/spec traceability, all applicable automated suites passing, native manual acceptance on the support matrix, no unresolved critical/high security findings, documented known limitations, migration/uninstall verification, and an operational rollback/support plan.
