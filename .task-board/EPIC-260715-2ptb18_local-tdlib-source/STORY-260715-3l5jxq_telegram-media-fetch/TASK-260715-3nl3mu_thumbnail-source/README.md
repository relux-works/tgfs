# Implement thumbnail and preview source

## Description
Expose existing Telegram thumbnails/minithumbnails without hydrating full media where allowed.

## Scope
Capability, cache, privacy, and unsupported fallback.

## Acceptance Criteria
Thumbnail requests are bounded, correctly typed/versioned, restriction-aware, and never force full media download unintentionally.
