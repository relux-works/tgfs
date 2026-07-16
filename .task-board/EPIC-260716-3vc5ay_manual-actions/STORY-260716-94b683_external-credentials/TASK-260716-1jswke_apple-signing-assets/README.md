# TASK-260716-1jswke: apple-signing-assets

## Description
Provision Apple Developer Program membership, signing certificates, App IDs, App Group identifiers, File Provider entitlements, and provisioning profiles for the macOS and iOS apps and their File Provider extensions. Store signing material outside the repository and expose it to CI via secrets.

## Scope
(define task scope)

## Acceptance Criteria
Signed macOS and iOS builds including File Provider extensions succeed in CI with valid entitlements; no signing material in the repository.
