# Installed projection-node SQLite degradation analysis

## Context

Board task: `BUG-260827-1tv0nl`.

Build 146 migrated the retained profile from schema 19 to schema 25, restored one
authorized namespace, and reached durable Finder readiness. It nevertheless kept
publishing retryable `projection-node-sqlite-storage` degradation during all six
bounded health probes. This analysis used only an SQLite backup of the preserved
failed candidate state and aggregate queries. No installed application, live
profile, authorization state, File Provider domain, or retained source session
was opened or mutated.

The preserved outcome used as the installed boundary is
`BUG-260827-2pr4vb_build146-readiness-red.json`. It records schema 25,
`quick_check=ok`, ready Finder content, a nonempty first page, and six uncleared
degradation probes.

## Findings

### Root cause

The failure is a deterministic SQLite foreign-key violation in bounded,
chat-scoped startup projection convergence.

The projection entry point chooses a chat-scoped pass when **any** stored
appearance exists for the canonical chat. It falls back to the safe full-scope
pass only when the stored appearance set is empty
([namespace.rs, `reconcile_chat_projection_txn`](../crates/gramdrive-ffi/src/namespace.rs#L4874)).

That predicate is too weak. A retained chat can currently belong to more than
one provider list while only a subset of those list appearances is stored. In a
chat-scoped pass:

1. The tree correctly contains every currently expected list appearance.
2. The pass deliberately does not reconcile a whole list, because doing so
   would tombstone siblings that were not loaded.
3. `refresh_directory_row` sees the missing chat-appearance row and returns
   without creating it; its own contract says creation belongs to a full pass
   ([namespace.rs, `refresh_directory_row`](../crates/gramdrive-ffi/src/namespace.rs#L5442)).
4. The caller nevertheless descends into that missing chat appearance and
   invokes `reconcile_nodes` for its children
   ([namespace.rs, targeted traversal](../crates/gramdrive-ffi/src/namespace.rs#L5293)).
5. The first generated-document child upsert references the absent chat parent,
   so SQLite rejects the real `WriteTxn::upsert_item` operation with a foreign-key
   constraint failure.

The existing publisher classifies unrecognized SQLite failures from this upsert
as `projection-node-sqlite-storage`, retryable true
([namespace.rs, classifier](../crates/gramdrive-ffi/src/namespace.rs#L670)). The
namespace worker publishes that failure and exits
([namespace.rs, worker lifecycle](../crates/gramdrive-ffi/src/namespace.rs#L496)).

### Deterministic preserved-profile reproduction

The task-private probe drove the production `converge_projection_slice` entry
point, which selects one bounded listed chat, reconciles it, and advances the
durable cursor only after a successful transaction
([namespace.rs, convergence entry point](../crates/gramdrive-ffi/src/namespace.rs#L1073)).

Privacy-safe results:

| Probe | Result | Exit |
| --- | --- | ---: |
| Preserved clone integrity and schema | schema 25; quick check OK; zero FK violations; zero migration-progress or repair-marker rows | 0 |
| Projection invariants before replay | zero scope mismatches, live orphan parents, sibling duplicate groups, or appearance duplicate groups | 0 |
| Full-scope deep projection replay | Succeeded; therefore the schema and complete-tree writer are sound | 101 expected-red assertion (the probe expected failure) |
| First bounded chat-scoped slice | Succeeded with one processed chat | 101 expected-red assertion |
| First 100 bounded slices | All succeeded; the temporary probe bound expired | 101 expected-red assertion |
| Complete bounded startup replay | Reproduced on slice 992: `GeneratedDoc`, missing parent, SQLite foreign-key constraint | 0 |
| Probe removal and source restoration | Temporary instrumentation removed; source matched its saved copy; `git diff --check` passed | 0 |

The successful reproduction is not a helper-only test: it calls the same
`converge_projection_slice` production boundary used by namespace startup.
No identifier, display name, path, message, source payload, or row was printed.

### Why ready Finder health coexists with degradation

Schema 24 intentionally persists namespace usability independently from later
source convergence
([migration registry](../crates/gramdrive-state/src/migrate.rs#L243)). On
restart, Swift restores that durable-ready fact. A later retryable namespace
failure is retained as source degradation while the already-published Finder
tree remains usable. Only a subsequent real `.ready` callback removes the
degradation
([AgentLifecycle.swift, progress lifecycle](../apple/GramDriveSupport/Sources/GramDriveAgentCore/AgentLifecycle.swift#L1395)).

This separation is truthful and should remain. Clearing degradation merely
because first-page enumeration succeeds would suppress a real source failure.

### Migration and retained-session classification

The schema-19-to-25 migration is not the faulty owner:

- The preserved file is fully at schema 25, passes quick/FK checks, and has no
  unfinished migration or repair marker.
- v20 adds only the generated-document lookup index, v21 rebuilds the provider
  view, v24 adds durable readiness, and v25 adds the live attachment size lookup
  index ([migration registry](../crates/gramdrive-state/src/migrate.rs#L205)).
- A full deep replay succeeds against the exact preserved schema-25 clone.
- The same static clone reproduces only when driven through bounded chat-scoped
  startup convergence.

Therefore migration is neither necessary nor sufficient to trigger the fault.
The retained source session made the large, multi-membership namespace available
at startup, but private session bytes are not needed for a regression fixture.
The minimal trigger is durable schema-25 state with one currently expected chat
appearance missing while at least one other appearance of the same chat exists,
followed by the real bounded convergence entry point.

## Smallest safe recovery boundary

Change only the selection boundary in `reconcile_chat_projection_txn`:

- Compare the current expected list-appearance set for the target chat with the
  stored live appearance set.
- Use the chat-scoped fast path only when every expected chat appearance exists.
- If any expected appearance is missing (or the stored appearance read fails),
  run the existing full-scope deep reconciliation in the same transaction.
- Do not clear or rewrite authorization, namespace identity, profile data, or
  readiness. Do not classify the missing parent as dataless.

The full-scope path is already the owner that can safely create a missing chat
appearance because it sees all siblings and performs stable name resolution.
After that transaction succeeds, normal convergence reaches `.ready`; the
existing Swift lifecycle then clears the prior degradation based on observed
storage health, not on a timer or suppression rule.

Skipping children of the missing appearance is rejected: it avoids the FK but
would advance convergence while leaving a required provider branch absent.
Creating one chat appearance locally is also rejected: the source comment
correctly states that sibling naming requires the full list.

## Regression and negative-proof directive

Add a Rust test beside the namespace projection tests that drives
`converge_projection_slice`, not a new helper:

1. Seed one authorized account and a chat with two current list memberships.
2. Materialize only one chat appearance and its descendants, leaving the other
   expected chat appearance absent.
3. Publish durable readiness with convergence incomplete.
4. Pre-fix assertion: the real entry point returns retryable
   `projection-node-sqlite-storage` from the generated-document FK violation.
5. Post-fix assertions: the slice succeeds, both expected chat appearances and
   their generated-document children exist, no unrelated sibling is tombstoned,
   the durable cursor advances, and a repeated slice/restart is idempotent.
6. Add the fresh-profile shape (no stored appearance) to retain the existing
   full-rebuild behavior, plus the current complete-appearance fast path.

Negative proof must narrow the new predicate from “any expected appearance is
missing” back to “all appearances are missing” (the current bug). Run the named
test uncached and require it to fail. This proves the partial-appearance class,
not merely that a fallback exists.

After implementation, run the focused Rust negative proof and migration/fresh
profile tests, then the configured core, Apple, repository, security,
live-content, and package gates as standalone commands with real exit codes.
Installed acceptance still requires a reviewed exact-main signed/notarized
candidate and the unchanged retained-session readiness/hydration/document gates.

## Fact-checking and rejected hypotheses

| Claim | Evidence | Status |
| --- | --- | --- |
| The database is corrupt | quick check OK; zero FK-check rows | Rejected |
| A migration is unfinished | schema 25; zero migration-progress and repair-marker rows | Rejected |
| A uniqueness collision owns the category | no duplicate groups; classified error is FK, not UNIQUE | Rejected |
| Full projection cannot represent the retained state | full-scope replay succeeds | Rejected |
| Startup/chat-scoped replay is required | full replay succeeds; bounded replay reproduces deterministically | Verified |
| Finder readiness may truthfully coexist with source degradation | durable readiness and degradation are separate production state; only `.ready` clears degradation | Verified |

## References

- `BUG-260827-2pr4vb_results.md` and
  `BUG-260827-2pr4vb_build146-readiness-red.json` (task-board outcome resources).
- [Rust namespace projection and publisher](../crates/gramdrive-ffi/src/namespace.rs).
- [Swift namespace degradation lifecycle](../apple/GramDriveSupport/Sources/GramDriveAgentCore/AgentLifecycle.swift).
- [Forward schema migration registry](../crates/gramdrive-state/src/migrate.rs).

## Implementation and verification

`reconcile_chat_projection_txn` now derives the expected appearance identities
from the current Main, Archive, Stories, and configured-folder memberships and
compares them with stored live appearances. It retains the bounded chat-scoped
path only when the expected set is a subset of the stored live set. Any missing
expected parent uses the existing full-scope deep reconciliation in the same
transaction. A failed appearance or catalog read still returns a retryable
storage failure; it is never interpreted as an empty or complete set.

The permanent regression drives `converge_projection_slice` against a durable
synthetic store with two current memberships, one materialized appearance, and
an unrelated folder sibling. It proves that both chat parents and their
generated-document children are live, the sibling is not tombstoned, the
cursor advances only after the repair commits, and a reopened completed store
is journal-idempotent. A second production-entry regression preserves the
fresh-profile/no-appearance path. Existing bounded convergence coverage retains
the complete-appearance fast path.

The negative proof replaced the completeness predicate with the narrower old
condition, “any live appearance exists,” and ran the named regression in a new
`CARGO_TARGET_DIR`. It failed with exit 101 at the real convergence call and
reported retryable `projection-node-sqlite-storage`. Restoring from the saved
copy produced identical SHA-256 digests and `cmp` exit 0; the named regression
then passed.

All developer-owned gates passed with real exit 0: the focused 182-test FFI
package, formatter, focused Clippy, configured core (6/6), repository (2/2),
Apple (2/2), security (1/1), live-content (1/1), and arm64 core package plus
Swift consumer verification. The core suite includes the shipped forward
migration fixtures and schema-19/v20 through schema-25 migration tests. No
installed app, retained profile/session, File Provider domain, Keychain, or
CloudStorage state was opened or mutated; signed exact-main installed rerun
remains a post-review/release responsibility.
