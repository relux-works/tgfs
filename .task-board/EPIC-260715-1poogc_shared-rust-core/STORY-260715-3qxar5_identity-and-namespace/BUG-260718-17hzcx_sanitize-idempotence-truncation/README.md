# BUG-260718-17hzcx: sanitize-idempotence-truncation

## Description
gramdrive_model::naming::sanitize violates its idempotence property (naming_properties::sanitize_is_idempotent) for long file names where stem truncation changes the extension decision between passes. Found by a random proptest seed during TASK-260715-1onbmf verification; not related to that task code.

Root cause: prepare() decides the extension split on the untruncated name; compose() then truncates the stem and unconditionally trim_edges()-trims trailing dots. Pass 1: tail after the last dot is longer than MAX_EXTENSION_BYTES (17) so split_extension declines, and truncation drops trailing chars. Pass 2 (on the pass-1 output): the same tail is now within 17 bytes so split_extension accepts it, the stem now ends with a dot, trim_edges eats that dot, and stem+extension re-joins with one dot where the input had two — sanitize(sanitize(x)) != sanitize(x).

Deterministic repro: cc ce9a3f44b0ed1b83417c51fab88ac58776fcfafed29e10935f16a222b5bdad9f (proptest-regressions entry for naming_properties; append it to crates/gramdrive-model/tests/naming_properties.proptest-regressions to replay). Shrunken input is a long name ending in mCP..<emoji><ZWJ><ZWJ>\\sigma-1<... of kind File; the divergence is CP..<emoji> vs CP.<emoji> in the two passes.

Fix directions (owners call): (a) iterate prepare+compose to a fixpoint inside sanitize (converges: every unstable pass shortens the name); (b) make the extension decision stable under truncation, e.g. re-split after the cut; (c) restrict plausible extensions further (does not fully close the cliff). The regression entry was intentionally not committed with TASK-260715-1onbmf to keep that diff scoped; whoever fixes this should commit the entry alongside the fix.

## Scope
(define bug scope / affected area)

## Acceptance Criteria
(define fix acceptance criteria)
