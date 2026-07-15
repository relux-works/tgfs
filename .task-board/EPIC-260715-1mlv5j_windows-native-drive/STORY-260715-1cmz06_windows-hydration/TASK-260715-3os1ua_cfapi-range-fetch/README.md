# Implement CfAPI range fetch

## Description
Bridge requested ranges to shared transfer engine and complete them with correct offsets, progress, and transfer keys.

## Scope
Sparse ranges, concurrent readers, cancellation, version race, disk/source failure.

## Acceptance Criteria
Native integration fixtures verify exact data and no callback/handle leak; partial data never becomes current.
