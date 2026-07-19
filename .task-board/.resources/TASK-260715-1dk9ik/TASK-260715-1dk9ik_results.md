# TASK-260715-1dk9ik — macOS signing, entitlements, packaging: results

Status: ready for review (board `to-review`).

## What was built

A reusable local pipeline that assembles, signs, verifies, and notarizes the
GramDrive desktop app — the barycenter "one entrypoint" pattern, mirroring
`.scripts/packaging` and `.scripts/tdlib`. The tag-triggered `release.yml` that
*invokes* it is a separate task (TASK-260715-3bhbkv).

| Deliverable | Path |
|---|---|
| Packaging pipeline | `.scripts/apple-app/build_app_bundle.py` |
| Pipeline docs | `.scripts/apple-app/README.md` |
| Self-tests (41, faked subprocess, run by repo gate) | `.scripts/tests/test_build_app_bundle.py` |
| Appex entry point | `apple/GramDriveSupport/Sources/GramDriveFileProviderExtensionApp/main.swift` + `Package.swift` target/product |
| Make targets | `Makefile`: `package-app`, `package-app-notarize` |
| Agent identifier row | `.spec/platform-requirements.md` (closes LOGBOOK 172) |
| Findings/decisions | `LOGBOOK.md` 2026-07-19 2110 |

## Bundle produced (macOS 14+ arm64, POL-5/DEC-017)

```
GramDrive.app/Contents/
  Info.plist                          com.reluxworks.gramdrive
  PkgInfo                             APPL????
  MacOS/GramDrive                     menu-bar companion shell (gramdrive-companion)
  MacOS/gramdrive-agent               launchd-run engine-hosting agent
  Library/LaunchAgents/com.reluxworks.gramdrive.agent.plist
  PlugIns/GramDriveFileProvider.appex/Contents/
    Info.plist                        com.reluxworks.gramdrive.fileprovider + NSExtension
    MacOS/GramDriveFileProvider       NSFileProviderReplicatedExtension host
```
Plus `GramDrive-<version>.dmg` (signed, + stapled when notarized), `manifest.json`,
`CHECKSUMS.sha256`, generated `entitlements/`, `logs/`. Output: `.temp/app-packaging/`.

## Entitlements (Developer ID, hardened runtime)

| Binary | Bundle id | Entitlements | Sandbox |
|---|---|---|---|
| App | `com.reluxworks.gramdrive` | app-groups=[262RZ595FP.com.reluxworks.gramdrive] | no |
| Agent | `com.reluxworks.gramdrive.agent` | app-groups=[...] | no |
| Appex | `com.reluxworks.gramdrive.fileprovider` | app-sandbox=true + app-groups=[...] | yes |

- Hardened runtime via `codesign --options runtime`; trusted timestamp via `--timestamp`.
- **No `get-task-allow`** anywhere (SwiftPM's debug default would fail notarization;
  the verify step dumps and asserts its absence).
- Team-prefixed App Group only — no portal registration, no provisioning profile under Developer ID.

## Acceptance criteria — evidence

AC: "Automated build produces verifiable signed artifacts without embedded
credentials and passes entitlement validation." Proven end-to-end on this host
(Xcode 26.5, Developer ID Application: Relux Works, LLC (262RZ595FP)):

- `codesign --verify --deep --strict --verbose=2 GramDrive.app` → exit 0, "valid on disk", "satisfies its Designated Requirement".
- Hardened runtime `flags=0x10000(runtime)` on app + agent + appex; `TeamIdentifier=262RZ595FP`.
- Entitlements dumped from the real signature and asserted: app-groups present everywhere, app-sandbox only on the appex, get-task-allow absent.
- Notarization: `xcrun notarytool submit --keychain-profile gramdrive-notary --wait` → submission `e012f0ae-c535-408d-9192-985d856f4954`, status **Accepted**.
- `xcrun stapler staple` + `stapler validate` → "The validate action worked!".
- `spctl --assess --type exec` on the stapled app → **accepted, source=Notarized Developer ID**.
- No embedded credentials: identity resolved from the keychain (name/team recorded in the manifest, never key material); notarization via keychain profile (submission id/status recorded). Self-tests assert the manifest carries no `PRIVATE KEY` / `p12` / `password` / `AuthKey` strings.

## Verification commands run

- `swift build -c release` for all three products — OK.
- `swift test` (apple package) — **252/252 across 47 suites** (Package.swift change is clean).
- `python3 -m unittest discover -s .scripts/tests` — **41 app-packaging tests pass** (part of the repo gate).
- `make check` repo suite (`run_automated.py --suite repo`) — **2/2** (traceability survives the spec edit; scripts gate green).
- `make check` core suite — see board note / gate provenance `.temp/acceptance/local-core`.
- Real `make package-app` and `make package-app-notarize` — both PASSED.
- `nm` on the appex binary — `_NSExtensionMain` referenced, principal class present (not dead-stripped).

## Scope boundaries held (no forced fits)

- TDLib-in-agent dylib embedding / rpath / `disable-library-validation` deferred to the release/TDLib task — the current agent links only the Rust core staticlib, so nothing was hacked in.
- The release CI workflow (cert import from `MACOS_CERT_P12`, tag trigger, attestations, SBOM) is TASK-260715-3bhbkv.
- Functional File Provider domain-load/live proof is acceptance TASK-260715-3oe2nr; a real signed bundle embedding the appex now exists to unblock it.
