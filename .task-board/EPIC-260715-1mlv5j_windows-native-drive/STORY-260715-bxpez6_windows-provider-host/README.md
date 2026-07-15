# Windows provider and engine host

## Description
Host CfAPI callbacks, shared Rust core, and LocalTdlibSource in a durable background process.

## Scope
Process/service/tray lifecycle, startup, network/power changes, shutdown, health, and multiple accounts path.

## Acceptance Criteria
Host reconnects and recovers transfers after crash/reboot, exposes health, and never blocks CfAPI callbacks unboundedly.
