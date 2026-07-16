# CI reference: relux-works/barycenter

Canonical CI/release pattern for this project (owner directive, 2026-07-17). Source: https://github.com/relux-works/barycenter `.github/workflows/`.

## ci.yml pattern
- One job per component; runners: ubuntu-24.04 (Go/portable), macos-15 (Swift/Xcode), windows-2025 (packaged probes only).
- Every job runs a PINNED acceptance suite through one entrypoint script (`scripts/acceptance/run_automated.py --suite <x> --require-clean --run-id ci-<x>`), not ad-hoc test commands.
- Acceptance provenance from `.temp/acceptance/<run-id>` is uploaded as an artifact (`if: always()`, `if-no-files-found: error`, retention-days 14).
- `permissions: contents: read` at workflow level; `fetch-depth: 0` where history matters.
- Blind cross-build gates: targets without a native runner get a cross-compile job (CGO_ENABLED=0 GOOS=... in barycenter); real-runner packaged probes verify signing/packaging contracts separately.

## release.yml pattern
- Tag-triggered (`v*`); permissions: contents write, id-token write, attestations write (GitHub artifact attestation).
- macos-15: import Developer ID Application cert "Relux Works, LLC (262RZ595FP)" from secret MACOS_CERT_P12 into a temp keychain; hardened runtime; build .app; dmg via hdiutil; codesign --timestamp the dmg; notarize + staple.
- Version stamping from the tag; CFBundleVersion = git rev-count (Sparkle ordering).

## GramDrive-specific additions
- Notarization: keychain profile `gramdrive-notary` (ASC API key 52TSFQH37D, key at ~/.private_keys) — already validated locally; CI uses the ASC key from repo secrets.
- Telegram api_id/api_hash: local dev reads Keychain service `gramdrive-telegram` (accounts api_id/api_hash); CI injects via repo secrets. Never in the repository or logs.
- License/SBOM gate per POL-6 (permissive-only deps) is part of core-ci.
