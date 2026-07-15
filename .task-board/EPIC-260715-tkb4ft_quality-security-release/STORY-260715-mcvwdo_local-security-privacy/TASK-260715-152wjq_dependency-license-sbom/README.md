# Implement dependency, license, vulnerability, and SBOM controls

## Description
Scan Cargo/Go/native dependencies, pin provenance, enforce approved licenses, generate SBOM, and handle advisories.

## Scope
CI and release artifacts; GPL/AGPL reference-only boundary.

## Acceptance Criteria
Build fails per approved severity/license policy and release artifact has attributable SBOM.
