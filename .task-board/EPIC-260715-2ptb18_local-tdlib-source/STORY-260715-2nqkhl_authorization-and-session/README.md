# Authorization and session lifecycle

## Description
Implement normalized auth state machine for phone, code, email where applicable, two-step password, QR/future states, errors, logout, and revocation.

## Scope
Containing native applications plus shared non-secret state.

## Acceptance Criteria
All TDLib authorization states have explicit UX/error mapping; secrets use secure storage; interrupted auth resumes safely; logout cleanup passes.
