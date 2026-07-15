# Integrate session encryption and secure key references

## Description
Store TDLib database encryption keys and app credentials through platform secret-store abstractions.

## Scope
Key creation, retrieval, rotation expectations, recovery, and logout deletion.

## Acceptance Criteria
No secret enters logs/config/git; missing/corrupt key fails safely; cleanup and multi-account isolation tests pass.
