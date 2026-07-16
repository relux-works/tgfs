# TASK-260717-3dvved — Review verdict 3: ACCEPTED (→ done)

Reviewer: reviewer (claude), 2026-07-17. Read-only; no files changed by this review.

## Verdict summary

Both requested fixes landed exactly as specified, the blocking F2 failure path is **closed**, and every
verification claim in the rework evidence holds when re-run independently. Accepting.

The implementer also made the right structural call: both fixes *remove* the normative detail rather
than restate it, leaving `platform-requirements.md:15` as the single owner. That is what actually
prevents a fourth cycle — the 0242/0248 split-fix happened precisely because the same rule was
asserted in two files. Deferral over duplication is the correct lesson and it was recorded in LOGBOOK
0255 rather than left implicit.

## Independent verification (all PASS — re-run, not trusted from the evidence doc)

| Claim | Method | Result |
|---|---|---|
| F2 fixed — `architecture.md:76` | read file | Reads "derived from the `com.reluxworks.gramdrive` namespace … see `platform-requirements.md` § Identifier and naming convention for the exact per-platform forms, including the Apple-mandated App Group prefixes." Matches the prescribed fix ✓ |
| F2 — no App Group enumeration survives | `grep -rniE 'app group'` over `.spec/ docs/ README.md SECURITY.md` | 12 hits, **zero** assert a `.*` prefix rule over App Groups. Defect class fully eliminated ✓ |
| F3 fixed — `README.md:15` | read + `git diff` | "every shipped identifier is derived from the `com.reluxworks.gramdrive` namespace" ✓ |
| Deferral target is real | `grep -n "Identifier and naming convention"` | `platform-requirements.md:13` exists ✓ |
| **Deferral target actually delivers** | read `platform-requirements.md:13-28` | :15 states the namespace rule + Apple's mandatory `group.`/team-ID prefix; table :21-24 gives per-platform forms; :23 marks `262RZ595FP.com.reluxworks.gramdrive` as "the entitlement form v1 ships". The pointer is not a dangling promise ✓ |
| Consistency with macOS paragraph | read `architecture.md:80` | Reader hitting the App Group container instruction two lines below now follows the pointer to the correct shipped form. F2's failure path closed ✓ |
| Validator | `python3 .scripts/validate_traceability.py` | OK 201/201, exit 0 — matches baseline ✓ |
| `tgfiles` | grep over specs/docs/README/SECURITY | 1 hit — DEC-019 rationale only ✓ |
| Mis-cased gramdrive / "gram drive" | grep | 0 hits ✓ |
| `tgfs` as product name | grep | 7 hits, all legitimate codename/repo-path/collision-rationale ✓ |
| Repo NOT renamed | `git remote -v` | `git@github.com:relux-works/tgfs.git` ✓ |
| Scope discipline | `git diff --stat` | `architecture.md` +2, `README.md` reworded. `policies.md` and `platform-requirements.md` identifier rule untouched per scope ✓ |

## Surviving `com.reluxworks.gramdrive.*` hits — each re-checked, none are defects

- `.spec/policies.md:64` — "Bundle/package identifier prefix" only. Accurate (bundles/packages genuinely
  do use it), and **not in `git diff`** → pre-existing, confirmed independently rather than taken on
  faith. Correctly untouched.
- `docs/GLOSSARY.md:10`, `decisions.md:28`, `RISK_REGISTER.md:21`, `OPEN_QUESTIONS.md:36` — benign
  definitional/pointer glosses; none enumerate App Group; none is a normative identifier rule an
  implementer would source entitlements from. Correctly untouched.
- `LOGBOOK.md` — quotes the old text as historical record. Correct as-is.

## Non-blocking observations (do NOT reopen this task for these)

- **`docs/GLOSSARY.md:10`** — "identifier prefix `com.reluxworks.gramdrive.*`" is, strictly, the same
  overclaim in miniature: `262RZ595FP.com.reluxworks.gramdrive` does not carry it as a *prefix*. Two
  prior reviews and the rework scope ruled it a non-defect and I agree — it is a glossary gloss of a
  product name with no failure path, and no implementer sources entitlements from GLOSSARY.md. Noting
  it only so the next editor of that line knows it was seen and consciously left, not missed.
- **`.spec/platform-requirements.md:26`** — the macOS 15 vs 15+ *deployment target* imprecision from
  review-2. Correctly not actioned: scope forbade touching the file, and row :23 already says "macOS 14
  deployment target" so there is no failure path. Left OPEN in LOGBOOK 0255 for the next editor of that
  section. Right call — reopening an accepted file for a no-failure-path nit would have risked a fresh
  contradiction for zero gain.

## Definition of Done

- [x] All user-visible naming in `.spec/`, `README.md`, `docs/` uses GramDrive; `tgfs` codename refs intact per DEC-019
- [x] Identifier convention recorded in platform-requirements (owning section) + architecture (pointer)
- [x] Repository NOT renamed; stale-name greps clean, evidence attached
- [x] `.scripts/validate_traceability.py` passes (201/201, exit 0)
- [x] Docs consistent (no product code yet — planning phase)
- [x] Result linked as task-scoped outcome resources
- [x] Findings recorded in LOGBOOK (0255)
- [x] Implementation matches AC
- [x] Solution fits project architecture — single-owner normative rule + deferral is the right shape
- [x] Tests green — validator is the test surface for this task

## Carried forward — NOT blockers for this task, need coordinator action

1. **`my.telegram.org` app title still legacy `memori`.** Flagged by three consecutive reviews and
   **still has no board item.** I verified this directly: `task-board grep` finds it only in this
   task's resources and in the progress note of an already-`done` task
   (`TASK-260716-1iypv4:24`). That is exactly the "note that decays" failure — the one place it is
   recorded is a closed task nobody will reopen. It is outside this task's AC (specs/README/docs
   naming) so it does not block acceptance, and creating the item is coordinator work, not reviewer
   work. **Recommend the coordinator create it under EPIC-260716-3vc5ay / STORY-260716-94b683**
   (human-only, my.telegram.org login required).
2. **POL-7 trademark / handle / domain check before public release** — human-only, still open. Properly
   tracked in `RISK_REGISTER.md:21` (R-015 residual), `README.md:15`, and DEC-019, so this one is not
   at risk of decaying.
