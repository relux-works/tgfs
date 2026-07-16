# TASK-260717-3dvved — rework round 2 evidence (F1b twin)

Scope: 2 lines. Both fixes remove the App Group enumeration and defer to the
section that owns the per-platform detail (`platform-requirements.md:15`, already
accepted). `platform-requirements.md` was NOT touched, per scope.

## F1b-1 (BLOCKING) — .spec/architecture.md:76

Before:
> All shipped bundle, package, App Group, and sync-root identifiers use the
> `com.reluxworks.gramdrive.*` prefix, and the drive presents as GramDrive
> (POL-7); see `platform-requirements.md` § Identifier and naming convention.

After:
> Shipped identifiers are derived from the `com.reluxworks.gramdrive` namespace,
> and the drive presents as GramDrive (POL-7); see `platform-requirements.md`
> § Identifier and naming convention for the exact per-platform forms, including
> the Apple-mandated App Group prefixes.

Contradiction closed: no longer asserts a rule that `262RZ595FP.com.reluxworks.gramdrive`
(same spec, :23) violates, and no longer conflicts with the macOS App Group
container instruction in the paragraph directly below it (:80).

## F1b-2 — README.md:15

Before: "every shipped identifier uses the `com.reluxworks.gramdrive.*` prefix"
After:  "every shipped identifier is derived from the `com.reluxworks.gramdrive` namespace"

## Verification

1. `python3 .scripts/validate_traceability.py` → exit 0
   "OK: 201 requirements from .spec/ all mapped exactly once (166 active,
   24 deferred-platform, 10 deferred-optional, 1 future); 125 board elements
   referenced; no orphan references on the board."

2. Stale-name greps over `.spec/`, `README.md`, `docs/` → clean.
   - `tgfiles`, `gram drive`, mis-cased gramdrive: 0 hits.
   - `tgfs`: 6 hits, all codename-context (policies.md:65, decisions.md:28,
     platform-requirements.md:15, GLOSSARY.md:19, RISK_REGISTER.md:21, README.md:15).

3. `grep -rn 'com\.reluxworks\.gramdrive\.\*'` over specs/docs — surviving hits
   reviewed, none are the F1b defect:
   - `.spec/policies.md:64` — "Bundle/package identifier prefix" only. ACCURATE,
     ruled non-defect, pre-existing. Untouched.
   - `docs/GLOSSARY.md:10` — generic gloss, no App Group enumeration. Ruled
     non-defect. Untouched.
   - `.spec/decisions.md:28`, `docs/RISK_REGISTER.md:21`, `docs/OPEN_QUESTIONS.md:36`
     — same benign-gloss register as GLOSSARY.md:10; none enumerate App Group.
   - `LOGBOOK.md` — quotes the old text as historical record. Correct as-is.

## Optional item — NOT actioned (deliberate)

`platform-requirements.md:26` ("group. form applies once iOS or macOS 15+ enters
scope" — imprecise: macOS 15 is in scope; what is future is a 15+ deployment
target). The round-2 scope says do not touch platform-requirements.md, and the
reviewer marked this non-blocking with no failure path (row :23 already says
"macOS 14 deployment target"). Not fixed to avoid reopening an accepted file.
Logged in LOGBOOK 0255 as OPEN for whoever next edits that section.
