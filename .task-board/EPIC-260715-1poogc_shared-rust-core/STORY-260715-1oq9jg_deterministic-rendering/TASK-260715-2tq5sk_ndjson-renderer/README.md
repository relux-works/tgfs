# Implement versioned NDJSON renderer

## Description
Render lossless current message records with stable schema, field order, entity/provenance metadata, and unavailable states.

## Scope
Streaming generation and schema migration fixtures.

## Acceptance Criteria
Golden fixtures are deterministic and parseable; every specified message/attachment field is represented or explicitly unavailable.
