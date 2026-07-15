# Implement Linux user service lifecycle

## Description
Create daemon startup/shutdown/restart, signal handling, health, configuration, and secure runtime directories.

## Scope
Reference distribution and portable fallback documentation.

## Acceptance Criteria
Service survives restart/network loss, closes TDLib cleanly, and uses least-privilege paths/permissions.
