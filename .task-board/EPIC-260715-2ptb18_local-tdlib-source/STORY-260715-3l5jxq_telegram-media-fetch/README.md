# Telegram media metadata and download

## Description
Map attachments and implement TDLib file state, priority/download, resume, reference refresh, progress, cancellation, and restrictions.

## Scope
All supported accessible/saveable media types and large files.

## Acceptance Criteria
Shared range/transfer tests pass through adapter; protected/unavailable media fail explicitly; no whole-file memory buffering.
