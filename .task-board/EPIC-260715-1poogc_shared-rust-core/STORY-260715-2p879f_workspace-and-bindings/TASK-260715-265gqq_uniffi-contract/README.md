# Define UniFFI API and generation pipeline

## Description
Expose only provider-neutral asynchronous operations, records, errors, cancellation, and progress to Swift/Kotlin.

## Scope
UDL/proc-macro choice, generated bindings, threading, async, callbacks, and version compatibility.

## Acceptance Criteria
Swift and Kotlin smoke consumers compile; cancellation/errors round-trip correctly; Telegram and OS-native types are absent.
