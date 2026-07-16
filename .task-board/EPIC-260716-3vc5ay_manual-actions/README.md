# EPIC-260716-3vc5ay: manual-actions

## Description
Human-only actions extracted from delivery epics so every other epic can run autonomously in an agent loop. Contains: (1) product/architecture decisions requiring explicit human choice or approval, (2) external accounts, credentials and signing assets that only a human can register or purchase, (3) manual on-device release validation that cannot run unattended. Delivery epics reference these items via blocked_by links; once an item here is done, the dependent epic proceeds without further human stops.

## Scope
(define epic scope)

## Acceptance Criteria
Every delivery epic can proceed to done without human intervention beyond items tracked in this epic; each item here names its owner, evidence, and exact unblock condition.
