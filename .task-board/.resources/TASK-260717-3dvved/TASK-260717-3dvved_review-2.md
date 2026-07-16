# TASK-260717-3dvved — Review verdict 2: CHANGES REQUESTED (→ to-dev)

Reviewer: reviewer (claude), 2026-07-17. Read-only; no files changed by this review.

## Verdict summary

The F1 rework is **correct, well-sourced, and verified** — I re-ran every check independently and all
of it holds. The implementer also caught an error in my predecessor's own report (see "Credit" below).

One defect blocks acceptance, and it is **F1b again**: the reworked prefix rule was fixed in
`.spec/platform-requirements.md:15`, but this task authored the *identical* unsatisfiable claim in a
second spec file — `.spec/architecture.md:76` — and that instance was left standing. The fix is one
line.

## F1 rework independently verified (all PASS)

| Claim | Method | Result |
|---|---|---|
| F1a — v1 App Group added | read `.spec/platform-requirements.md:23` | `262RZ595FP.com.reluxworks.gramdrive` present, marked "the entitlement form v1 ships" ✓ |
| F1a — future form marked | `:24` | `group.com.reluxworks.gramdrive` marked "iOS and macOS 15+ / future, not used by v1" ✓ |
| F1b — prefix rule reworded | `:15` | "derived from the `com.reluxworks.gramdrive` namespace… App Groups additionally carry the platform-mandated prefix" — admits both `group.` and team-ID forms ✓ |
| DEC-017/POL-5 pointer | `:26` | present, names `TASK-260716-1jswke` as source ✓ |
| Values not invented | cross-read `TASK-260716-1jswke` progress:29 verbatim; team ID corroborated by :24/:28/:30 | exact match ✓ |
| Validator | `python3 .scripts/validate_traceability.py` | OK 201/201, exit 0 — matches baseline |
| `tgfs` as product name | `grep -rniE '\btgfs\b' .spec/ docs/ README.md SECURITY.md` | 8 hits, all legitimate codename/repo-path uses |
| `tgfiles` | same paths | 1 hit — DEC-019 rationale only. Correct |
| Wrong-cased gramdrive | grep excluding identifier forms | clean |
| Repo NOT renamed | `git remote -v` | `git@github.com:relux-works/tgfs.git` ✓ |

The unrequested heading fix ("Registered Apple identifiers" → "Apple identifiers" + per-row
**Registration** column) is a genuine improvement: the team-prefixed group is deliberately *not*
portal-registered, so filing it under the old blanket heading would have introduced a fresh
contradiction. Good catch, correctly reasoned.

## Credit — the implementer corrected the reviewer

My predecessor's report said the matrix is "macOS 14 (Sonoma), arm64 only — macOS 15+ explicitly out
of scope". That is **wrong**. DEC-017 (`.spec/decisions.md:26`) and POL-5 (`.spec/policies.md:47`)
say macOS **14+**. The implementer caught this, declined the literal wording, and labelled the row
"(macOS 14 deployment target)" instead of "on macOS 14" — correct: one build at deployment target 14
runs on 14 and 15, so it uses the team-prefixed form throughout. Right call, and it was documented
rather than done silently.

## Blocking finding — F2: F1b survives verbatim in `.spec/architecture.md:76`

**File:** `.spec/architecture.md:76` — **added by this task** (confirmed: `+` line in `git diff`).

The line reads:

> All shipped bundle, package, **App Group**, and sync-root identifiers use the
> `com.reluxworks.gramdrive.*` prefix, and the drive presents as GramDrive (POL-7); see
> `platform-requirements.md` § Identifier and naming convention.

This is F1b word-for-word, in a normative spec file:

1. It **explicitly enumerates "App Group"** in the list of things that "use the
   `com.reluxworks.gramdrive.*` prefix" — the exact assertion the last review established is
   unsatisfiable by any real App Group, since Apple mandates a `group.` or team-ID prefix *ahead* of
   the namespace.
2. It is **contradicted by the identifier it cross-references**: `262RZ595FP.com.reluxworks.gramdrive`
   does not use that prefix. The line literally points the reader at the section that now says the
   opposite.
3. The paragraph **immediately below it** is § Native layers → macOS, which instructs the implementer
   to "share durable metadata through an App Group container".

**Failure scenario:** an implementer opens `.spec/architecture.md` § Native layers to build the macOS
provider, reads line 76 as the normative identifier rule, reads the macOS paragraph two lines later
telling them to use an App Group, and writes `com.reluxworks.gramdrive.group` or
`group.com.reluxworks.gramdrive` into the macOS 14 entitlements — hitting the exact Developer ID
provisioning-profile wall that `TASK-260716-1jswke` already paid a human portal round-trip to
diagnose. This is the identical failure path the last review blocked on; the rework closed it in
`platform-requirements.md` and left it open in `architecture.md`.

This is not "re-doing the rollout" — the rework scope was narrowed to one file, and the implementer
complied precisely. But this task authored *both* copies of the claim in the same cycle, so fixing
one and leaving its twin is in-scope rework, not new work.

## Same class, lower severity — F3: `README.md:15`

> the public product name is **GramDrive**, and every shipped identifier uses the
> `com.reluxworks.gramdrive.*` prefix

Also added by this task. Same absolute claim, false for the one App Group v1 ships. Lower severity
than F2 — README is a summary, not a spec, and an implementer sourcing entitlements from the README
rather than the spec is not a plausible path. Fix it in the same pass since it is the same sentence.

## Requested changes (narrow — three lines, two files)

1. **`.spec/architecture.md:76`** (blocking) — reword so the App Group clause admits Apple's mandatory
   `group.` / team-ID prefixes, consistent with the now-correct `platform-requirements.md:15`. Simplest
   safe fix: drop the enumeration and defer to the spec section that owns the detail, e.g. "Shipped
   identifiers are derived from the `com.reluxworks.gramdrive` namespace and the drive presents as
   GramDrive (POL-7); see `platform-requirements.md` § Identifier and naming convention for the exact
   per-platform forms, including the Apple-mandated App Group prefixes."
2. **`README.md:15`** — same reword, summary register. "every shipped identifier is derived from the
   `com.reluxworks.gramdrive` namespace" is sufficient.
3. Re-run `python3 .scripts/validate_traceability.py` and the stale-name greps.

## Non-blocking findings

- **`.spec/platform-requirements.md:26`** — "`group.com.reluxworks.gramdrive` applies only once iOS or
  macOS 15+ enters scope" is imprecise: macOS 15 is *already* in v1 scope (matrix is 14+). What is
  future is a **deployment target** of 15+, not the OS version. Row `:23` already says "macOS 14
  deployment target", so no failure path — worth tightening opportunistically, not worth a cycle.
- **`.spec/policies.md:64`** — "Bundle/package identifier prefix: `com.reluxworks.gramdrive.*`" is
  **accurate** (bundles and packages genuinely do use it) and pre-existing, not authored by this task.
  Not a defect; do not "fix" it.
- **`docs/GLOSSARY.md:10`** — generic gloss, does not enumerate App Group. Acceptable.

## Carried forward (human-only, agreed with prior review)

- POL-7 trademark / handle / domain check before public release — still open.
- my.telegram.org app title still legacy `memori` (`TASK-260716-1iypv4`) — deserves its own board item
  rather than a note that decays. Still not created.

## Not a stop-the-line

Ordinary, recoverable doc rework against an already-accepted decision, using facts already established.
No human decision or external input needed → `to-dev`, not `blocked`.
