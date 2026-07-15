# macOS companion engine host

## Description
Host Rust core and LocalTdlibSource outside the File Provider extension with durable coordination.

## Scope
App/agent lifecycle, App Group, service boundary, startup, sleep/wake, network changes, and shutdown.

## Acceptance Criteria
Engine is available when provider needs it per supported lifecycle; restart/sleep tests recover; extension remains thin.
