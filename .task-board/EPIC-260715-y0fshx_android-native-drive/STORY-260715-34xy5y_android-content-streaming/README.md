# Android content, thumbnails, and offline cache

## Description
Implement openDocument streaming, cancellation, concurrent readers, thumbnails, pin/cache controls, and repair.

## Scope
Pipe/file descriptor strategy, TDLib/shared transfers, process death, and large files.

## Acceptance Criteria
Large files stream without whole-file buffering; readers receive correct versions; cancellation/process death leaves recoverable state.
