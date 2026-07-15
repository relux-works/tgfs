# Linux daemon and TDLib runtime

## Description
Host shared core/local source as a user service with account authorization, lifecycle, health, repair, and logging.

## Scope
systemd user service or selected equivalent, secrets, network/sleep, mount coordination, and removal.

## Acceptance Criteria
Daemon restarts safely, exposes health, protects credentials, and coordinates mount/unmount without data corruption.
