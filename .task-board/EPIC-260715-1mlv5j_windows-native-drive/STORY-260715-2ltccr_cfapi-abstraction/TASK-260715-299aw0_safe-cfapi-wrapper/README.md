# Implement safe CfAPI wrapper

## Description
Wrap registration/connect/disconnect/create/update/execute/pin/in-sync/status operations and callback data ownership.

## Scope
windows-rs raw bindings and Rust safety boundaries.

## Acceptance Criteria
Unsafe code is reviewed, callbacks cannot outlive connection/context, and API errors map to stable core categories.
