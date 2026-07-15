# Implement offline pin and eviction reconciliation

## Description
Map system pin/materialization state to shared cache policy and repair discrepancies.

## Scope
Pinned folders/files, quota pressure, system dehydration, and unpin.

## Acceptance Criteria
Pinned intent is durable, eligible content evicts only per policy, and reported state matches Finder/system state.
