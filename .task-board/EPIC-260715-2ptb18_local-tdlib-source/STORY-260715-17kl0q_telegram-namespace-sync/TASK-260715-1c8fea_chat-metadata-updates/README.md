# Implement chat metadata and list update mapping

## Description
Apply TDLib position/title/photo/folder/protection/deletion/left updates to normalized change stream.

## Scope
Transactional checkpoint and provider invalidation.

## Acceptance Criteria
Replay fixtures converge, reorder does not change canonical ID, and gap/restart behavior passes.
