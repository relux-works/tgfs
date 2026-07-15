# Implement fuser filesystem adapter

## Description
Translate FUSE requests, replies, interrupts, file handles, and mount options to shared core.

## Scope
Threading, cancellation, timeouts, attributes, and kernel cache policy.

## Acceptance Criteria
Filesystem stress tests show no hangs/leaks and correct metadata/error behavior.
