# TASK-260715-1dk9ik — Review verdict: ACCEPTED

Reviewer re-verified independently on this host (macOS, Xcode present). Read-only review; no code changed.

## AC
"Automated build produces verifiable signed artifacts without embedded credentials and passes entitlement validation." — **met.**

## Independently re-verified
- **Self-tests green.** `.scripts/tests/test_build_app_bundle.py` → 41/41 pass. Full repo test discovery → 174/174 pass.
- **Repo gate green.** `run_automated.py --suite repo` → 2/2 (traceability ok after the spec edit; scripts suite ok — it discovers the new tests). Provenance: `.temp/acceptance/review-1dk9ik`.
- **Package.swift wiring correct.** New `GramDriveFileProviderExtensionApp` executable target + `gramdrive-fileprovider` product, depends on the `GramDriveFileProvider` library. Signing order (BINARIES) is inside-out: appex → agent → app.
- **Principal-class name is real.** `public final class GramDriveFileProviderExtension` lives in module `GramDriveFileProvider`; the appex Info.plist `NSExtensionPrincipalClass` = `GramDriveFileProvider.GramDriveFileProviderExtension` — the correct Swift `Module.Class` runtime name resolvable by `NSClassFromString`.
- **Architectural fit (load-bearing).** Packaging writes the launchd plist to `Contents/Library/LaunchAgents/com.reluxworks.gramdrive.agent.plist`, which is exactly `SMAppServiceAgentLoginItem.defaultPlistName` in `LaunchAtLogin.swift`. BundleProgram `Contents/MacOS/gramdrive-agent` matches the assembled layout. The app can operate the login item it ships.
- **Credential-free.** Identity from keychain; notarization via `gramdrive-notary` keychain profile. Manifest records identity name/team + submission id/status only; self-tests assert no `PRIVATE KEY`/`p12`/`password`/`AuthKey` strings. Artifacts land in `.temp/app-packaging/` (gitignored) — nothing signed is committed.
- **Entitlements.** app-groups (team-prefixed `262RZ595FP.com.reluxworks.gramdrive`) on all three; app-sandbox only on the appex; no `get-task-allow` — and the verify step dumps+asserts each from the live signature, not just applies them.
- **Real end-to-end evidence.** `manifest.json` from an actual signed+notarized run: hardened-runtime cdhashes on all three binaries, notarization submission `e012f0ae-…` status **Accepted**, gatekeeper accepted.
- **Dependency satisfied.** Blocking TASK-260715-3s44pc (domain registration) is `done`.

## Design fit
Mirrors the barycenter one-entrypoint pattern (`.scripts/packaging/build_core_artifacts.py`, `.scripts/tdlib`): make target → single script → pure/tested argv builders + faked-subprocess tests runnable without Xcode/network/keys. Honest reproducibility claim (`byte_identical:false`, `attributable:true`) — correct for a trusted-timestamp signature. Scope boundaries held with no forced fits: release.yml → 3bhbkv, TDLib dylib embedding deferred, functional domain-load live-proof → 3oe2nr.

## Non-blocking follow-ups (do NOT block acceptance; for the release task)
1. **Staple the `.app`, not only the `.dmg`.** The pipeline notarizes+staples the dmg; the `.app` inside is left un-stapled. An app dragged out of the dmg then has no offline notarization ticket — first launch succeeds online (Gatekeeper queries Apple, which is why spctl reported accepted) but would be blocked offline. Recommended order for dmg distribution: staple the `.app` → build dmg from the stapled app → staple the dmg. Best folded into TASK-260715-3bhbkv (release workflow).
2. **Marketing version is `0.0.0` until a `v*` git tag exists** (intentional/honest). Release must tag first for a real `CFBundleShortVersionString`.
