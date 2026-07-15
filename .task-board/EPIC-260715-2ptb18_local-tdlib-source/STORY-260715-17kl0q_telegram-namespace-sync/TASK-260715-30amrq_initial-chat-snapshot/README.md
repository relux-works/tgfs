# Implement initial chat-list snapshot

## Description
Load complete list metadata without eagerly loading media/history and persist normalized chat appearances.

## Scope
TDLib getChats/list pagination and lazy chat detail resolution.

## Acceptance Criteria
Large synthetic/test account snapshot resumes, has no duplicates/gaps, and records exact source ordering metadata.
