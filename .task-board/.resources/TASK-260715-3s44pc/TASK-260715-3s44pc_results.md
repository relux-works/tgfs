# TASK-260715-3s44pc — File Provider domain registration: results

Date: 2026-07-19. Role: developer. Code-complete, all gates green,
**ready for review**.

## What landed

### 1. FFI contract 0.2.0 → 0.3.0 (additive): account snapshot reads

`crates/gramdrive-ffi/src/shared_state.rs` — the reads a provider host
maps File Provider domains from:

- **`SharedStateStore.accounts()`** — every configured account in stable
  identity order; **`account(account_id)`** — the point read a provider
  extension resolving its domain needs. Each one short WAL snapshot,
  same rules as the item reads.
- **`AccountInfo`** record: `account_id`, `source_kind`, `display_name`,
  `auth_state`, `namespace_version`, and `root_item_id` — the account
  root's item identifier derived *in Rust* via
  `ItemKey::Canonical(CanonicalKey::Account(..))`, so hosts never learn
  the identifier derivation scheme. **Never secret material**:
  `secret_ref` stays behind the boundary.
- **No writes added** — DEC-006's no-writes-over-FFI rule intact. Both
  bindings-smoke consumers updated to assert 0.3.0.

### 2. Swift `GramDriveFileProvider` — new library of `apple/GramDriveSupport`

- **`DomainIdentity`** — the identity rule. Identifier `account-<id>` is
  a pure function of the account's stable numeric identity — never
  display name, auth state, or namespace epoch — so reauthorization and
  namespace bumps can never move or recreate a domain (Finder state is
  keyed to the identifier; this is what "appears once and recovers"
  means). Parse-back is strict by round-trip (`account-007` refused): a
  foreign domain identifier can never alias a real account. Naming per
  POL-7: single account → exactly **GramDrive**; several → "GramDrive —
  <name>" in identity order, name collisions append the id, blank names
  fall back to "Account <id>". Total and deterministic.
- **`DomainReconciler`** — the idempotent converge-toward-desired pass
  behind every acceptance path (first run, restart, duplicate install,
  reauthorization, multiple accounts): pure `plan` (adds / renames /
  keeps / strays) + application through the registrar seam. Strays —
  registered domains no account explains — are *reported, never
  touched*: removal/repair is TASK-260715-gnat2x's.
- **`DomainRegistrar` seam + `SystemDomainRegistrar`** — the live
  implementation wraps `NSFileProviderManager` (`domains()`, upserting
  `add`). The seam deliberately has **no remove operation**, which makes
  "registration never destroys Finder state" structural rather than
  disciplined.
- **`DomainStartupReconcile`** — the launch-time pass: resolve container
  → open shared state `.provider` → reconcile → outcome
  (`skipped`/`reconciled`/`failed`) for the log. Never throws, never
  blocks startup. Wired into the companion shell's `init` (off-main,
  `os.Logger`), because the shell is the app that will embed the
  extension.
- **`GramDriveFileProviderExtension`** — the thin
  `NSFileProviderReplicatedExtension` skeleton (DEC-006: thin is the
  design, not a stage). `accountContext()` = parse domain identifier →
  open shared state as `.provider` (handle cached until `invalidate()`)
  → resolve account + root item id. Callbacks: unresolvable domain →
  `NSFileProviderError(.noSuchItem)`; resolvable domain →
  `CocoaError(.featureUnsupported)` until the enumeration/content
  stories land (STORY-260715-14k4l9, STORY-260715-14n7wp); storage
  failures pass through untranslated. **No TDLib by construction**: the
  target links only GramDriveSupport + the core bindings — there is no
  TDLib dependency to link.

### 3. Shared-state smoke: new `domains` step

`SharedStateSmoke --mode domains` + runner step: a separate provider
process reads the Rust-seeded account through the packaged artifact,
derives the desired domain (`account-7` / "GramDrive"), constructs the
real extension type against that domain, and resolves the account
context back to the **same root item id the Rust seeder printed** — the
cross-process proof that domain → account → shared-state wiring is real.

## AC evidence

| AC / scope path | Proof |
|---|---|
| Domain appears once with correct identity/name | Identity tests (stable derivation, strict round-trip); reconcile first-run test registers exactly once; smoke asserts `domain_id=account-7`, `domain_name=GramDrive` |
| Recovers after app/provider restart | Registration is durable in the system; launch-time pass re-runs into a settled no-op (repeat-pass test: zero registrar calls) |
| First run | `firstRun` test + startup pass over real (empty and seeded) shared state |
| Reauthorization | auth_state plays no part in identity/naming — tests prove identical desired sets and zero registrar calls across auth changes |
| Duplicate install | Same stable identifier + upserting `add` + idempotent pass — `repeatedPassIsIdempotent`; strays untouched test proves a foreign registration is never destroyed |
| Multiple accounts | Ordering + disambiguation tests; second-account arrival renames the first domain and adds the second under stable identifiers |
| Thin extension wired to shared state, no TDLib (DEC-006) | Extension resolves accounts through `.provider`-role reads only; target has no TDLib dependency; cross-process smoke proves the read chain |

## Verification

| Check | Result |
|---|---|
| `make check` (suite all) | **8/8 ok** — provenance `.temp/acceptance/local-all` |
| `cargo test -p gramdrive-ffi` | 26 green (3 new account-read tests) |
| `swift test` (apple/GramDriveSupport, arm64, macOS 14 floor) | **118/118** in 25 suites (28 new in 5 domain suites) |
| `swift build` (all targets incl. new library + companion wiring) | ok |
| `make package` | PACKAGING PASSED — artifact carries contract 0.3.0 |
| `make smoke-bindings` | SWIFT + KOTLIN SMOKE PASSED (0.3.0 asserted) |
| `make smoke-shared-state` | PASSED incl. new `domains` step |
| `make smoke-agent-lifecycle` | PASSED (regression) |

Logs: `.temp/TASK-260715-3s44pc/` (check-01, smoke-bindings-01,
smoke-agent-01, package-01), `.temp/shared-state-smoke/domains.log`.

## Platform constraints documented (not forced)

1. `NSFileProviderManager` domain management resolves the extension from
   the calling app's bundle → the reconcile call lives in the companion
   shell; live registration (signed app + embedded .appex) is provable
   only by the signing/packaging task (TASK-260715-1dk9ik).
   `SystemDomainRegistrar` is the only surface not unit-tested;
   everything above the seam is tested against fakes.
2. Swift tests cannot seed accounts (no writes over FFI, by design) —
   the seeded happy path is proven by the Rust FFI tests and the
   cross-process smoke, exactly like the shared-state layer itself.
3. The real `.appex` bundle (Info.plist principal class, entitlements)
   is packaging's; this task ships the principal-class implementation
   and the registration layer as libraries, matching the package's
   established product shape.

## Files

- New: `apple/GramDriveSupport/Sources/GramDriveFileProvider/
  {DomainIdentity,DomainRegistrar,DomainReconciler,DomainStartupReconcile,
  FileProviderExtension}.swift`,
  `apple/GramDriveSupport/Tests/GramDriveFileProviderTests/
  {DomainIdentityTests,DomainReconcilerTests,FileProviderExtensionTests}.swift`
- Modified: `crates/gramdrive-ffi/src/{shared_state,api}.rs`,
  `crates/gramdrive-ffi/README.md`, `apple/GramDriveSupport/
  {Package.swift,README.md}`, `apple/GramDriveSupport/Sources/
  {GramDriveCompanionMain/CompanionMain.swift,SharedStateSmoke/main.swift}`,
  `.scripts/smoke/{run_shared_state_smoke.py,swift/main.swift,kotlin/Main.kt}`,
  `LOGBOOK.md`
- Nothing committed (workflow: commits happen after human review).

## Out of scope (owned by siblings)

- Domain removal, stale-domain repair, logout/uninstall cleanup —
  TASK-260715-gnat2x (the registrar seam is deliberately remove-free).
- `NSFileProviderItem` mapping — TASK-260715-i3mp9x; enumeration —
  STORY-260715-14k4l9; content fetch — STORY-260715-14n7wp.
- Finder change signaling (`signalEnumerator`) — enumeration story.
- Signed bundle, entitlements, .appex embedding, live registration
  proof — TASK-260715-1dk9ik.
- Agent health `providerRegistrationState` wiring — the field stays
  honest `nil` until the domain status reporting lands with
  removal/repair.
