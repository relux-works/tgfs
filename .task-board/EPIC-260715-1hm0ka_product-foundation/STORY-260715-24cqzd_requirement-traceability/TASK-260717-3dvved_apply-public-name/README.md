# TASK-260717-3dvved: apply-public-name

## Description
Apply the accepted public name GramDrive (DEC-019) across specs, README, docs, and identifier conventions: bundle/package prefix com.reluxworks.gramdrive.*, user-visible strings, and store-listing placeholders. Repository/codename stays tgfs. Do not rename the repository.

## Scope
(define task scope)

## Acceptance Criteria
All user-visible naming in .spec/, README, and docs uses GramDrive; identifier convention com.reluxworks.gramdrive.* recorded in platform specs; repository stays tgfs; a grep for stale public-name usages is clean.
