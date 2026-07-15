# Implement device enrollment and revocable tokens

## Description
Issue scoped per-device credentials through an approved pairing/auth flow and support rotation/revocation.

## Scope
No Telegram key distribution; least privilege and secure storage on clients.

## Acceptance Criteria
Stolen/revoked/expired token tests fail closed; audit events are redacted; device removal terminates access.
