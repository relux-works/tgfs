# STORY-260716-94b683: external-credentials

## Description
External accounts, credentials, signing assets, and hardware that must be provisioned by a human before dependent engineering tasks can run: Telegram api_id/api_hash and test accounts, Apple Developer Program signing assets, Windows code-signing identity, physical test devices.

## Scope
(define story scope)

## Acceptance Criteria
All credentials/assets are provisioned, stored outside the repository, reachable by CI and dev builds, and documented with owner and rotation; dependent tasks are unblocked.
