# Remote device authentication and service security

## Description
Implement device enrollment/tokens, authorization middleware, key management, tenant isolation, rate limits, audit, deletion, and threat-model controls.

## Scope
SEC-040 through SEC-044 and hosted/self-hosted profiles.

## Acceptance Criteria
Threat model reviewed; token revoke is immediate; every endpoint is tenant-scoped; secrets are encrypted/rotatable; security tests pass.
