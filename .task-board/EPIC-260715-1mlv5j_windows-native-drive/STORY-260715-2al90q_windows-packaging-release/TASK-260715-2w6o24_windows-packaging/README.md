# Implement Windows packaging and signing

## Description
Configure selected packaging identity, service/startup, native dependencies, signing, provenance, and uninstall hooks.

## Scope
x64/arm64 per support decision and CI secret boundary.

## Acceptance Criteria
Artifacts are signed/versioned, contain no credentials, and cleanly preserve/migrate or remove state as specified.
