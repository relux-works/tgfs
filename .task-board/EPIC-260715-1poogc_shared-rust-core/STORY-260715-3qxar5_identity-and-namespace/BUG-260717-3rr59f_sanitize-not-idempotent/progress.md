## Status
backlog

## Assigned To
(none)

## Created
2026-07-17T13:52:54Z

## Last Update
2026-07-17T13:53:04Z

## Blocked By
- (none)

## Blocks
- (none)

## Checklist
(empty)

## Notes
Found during TASK-260715-1opnb2 review: make check rerun failed gramdrive-model naming_properties::sanitize_is_idempotent with a fresh proptest counterexample (57 successes, then failure; deterministic on seed replay). sanitize(sanitize(x)) != sanitize(x): first pass leaves a substring ..._ (dots adjacent to characters replaced with underscore, e.g. U+104394 followed by .. then |), second pass collapses the dot run differently (.._ became ._). Input class: mixed zero-width joiners (U+200C/200D), combining marks, forbidden punctuation (| ? < >) around dot runs. Violates the POL-1 stable-name promise: a name passing the policy twice (re-sync, rebuild) drifts. Sanitizer: crates/gramdrive-model/src/naming.rs:568 (TASK-260715-1ffbkg, commit b8d9b2b). Property: crates/gramdrive-model/tests/naming_properties.rs:171. Deterministic repro: append to crates/gramdrive-model/tests/naming_properties.proptest-regressions the cc line stored in TASK-260715-1opnb2_review.md (outcome resource on TASK-260715-1opnb2), then cargo test -p gramdrive-model --test naming_properties sanitize_is_idempotent. The regressions file in the working tree was restored to its handed-off state during review; the seed lives in the review resource.

## Precondition Resources
(none)

## Outcome Resources
(none)
