# TASK-260716-1q7o8y: windows-signing-identity

## Description
Acquire the Windows code-signing identity (certificate or Azure Trusted Signing) and the MSIX package identity required for the Cloud Files sync root host. Store signing material outside the repository and expose it to CI via secrets.

## Scope
(define task scope)

## Acceptance Criteria
A signed Windows package is produced in CI; identity ownership and renewal are documented.
