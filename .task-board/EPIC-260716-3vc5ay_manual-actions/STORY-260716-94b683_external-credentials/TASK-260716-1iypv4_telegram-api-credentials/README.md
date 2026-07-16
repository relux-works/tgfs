# TASK-260716-1iypv4: telegram-api-credentials

## Description
Register a production api_id/api_hash at my.telegram.org for this product and make them consumable by dev and CI builds without ever entering the repository. Test accounts are provisioned programmatically on the Telegram test DC during tdlib-configuration work (documented approach), so no separate human-owned test account is required.

## Scope
(define task scope)

## Acceptance Criteria
api_id/api_hash stored in macOS Keychain for local dev and as GitHub Actions secrets for CI; test-account approach documented; no secrets in the repository or logs.
