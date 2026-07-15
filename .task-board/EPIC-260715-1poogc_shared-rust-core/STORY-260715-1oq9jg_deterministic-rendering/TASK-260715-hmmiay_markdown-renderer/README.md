# Implement monthly Markdown renderer

## Description
Render human-readable bounded partitions with timezone, sender, entities, replies, topics, albums, reactions, service messages, and attachment links.

## Scope
Streaming/atomic generation and Unicode-safe output.

## Acceptance Criteria
Golden fixtures cover specified message types; unchanged inputs are byte-identical; links resolve to stable virtual items.
