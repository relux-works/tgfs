# Implement release signing and provenance

## Description
Release workflow mirroring relux-works/barycenter release.yml: tag-triggered, macos-15 runner, Developer ID Application identity (Relux Works LLC, team 262RZ595FP) imported from repo secret (MACOS_CERT_P12 pattern), hardened runtime, dmg build, codesign with timestamp, notarize+staple via the gramdrive-notary keychain profile / ASC API key, GitHub artifact attestations (id-token+attestations permissions), checksums, SBOM, changelog, rollback metadata, Sparkle-compatible versioning (CFBundleVersion from rev-count).

## Scope
Per-platform credentials and approval controls.

## Acceptance Criteria
Release artifacts verify independently, contain no development credentials/sessions, and are traceable to reviewed commit.
