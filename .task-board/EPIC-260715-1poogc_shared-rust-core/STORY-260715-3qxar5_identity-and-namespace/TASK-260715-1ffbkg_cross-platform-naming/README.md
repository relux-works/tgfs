# Implement safe naming and collision resolution

## Description
Sanitize untrusted Telegram names for the strictest target and resolve collisions from stable identity.

## Scope
Unicode normalization, reserved names, separators, controls, trailing characters, case collisions, and path budgets.

## Acceptance Criteria
Shared fixture corpus passes for Apple, Windows, Android, and Linux expectations; no traversal or unstable discovery-order suffixes.
