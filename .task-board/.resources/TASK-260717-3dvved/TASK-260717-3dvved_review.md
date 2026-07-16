# TASK-260717-3dvved — Review verdict: CHANGES REQUESTED (→ to-dev)

Reviewer: reviewer (claude), 2026-07-17. Read-only; no files changed by this review.

## Verdict summary

The naming rollout itself is **correct and well-evidenced** — I re-ran every claimed verification
independently and all of it holds. One defect blocks acceptance: the **new** identifier section this
task added to `.spec/platform-requirements.md` records the wrong App Group for the **only v1
platform**, and states a normative prefix rule that its own table contradicts.

The fix is ~3 lines in one file. Everything else stands as-is.

## Independently verified (all PASS)

| Claim | Method | Result |
|---|---|---|
| Validator unbroken | `python3 .scripts/validate_traceability.py` | OK 201/201, exit 0 — matches claimed baseline |
| No `tgfs` as product name | `grep -rniE '\btgfs\b' .spec/ docs/ README.md SECURITY.md` | 8 hits, **all** legitimate codename/repo-path uses (POL-7 text, DEC-019 rationale, GLOSSARY codename entry, advisory path, prior-art links, naming paragraphs) |
| No `tgfiles` | same paths | 1 hit — DEC-019 rationale only. Correct; the decision must retain why it was rejected |
| No wrong-cased gramdrive | grep excluding identifiers | clean |
| Board clean | `grep -rniE '\btgfs\b' .task-board --include='*.md'` | all hits are repo/codename refs (repo secrets, codename tasks). `STORY-260715-31thz2` correctly reads "Expose GramDrive roots…" |
| `.planning/` clean | grep | clean |
| Repo NOT renamed | `git remote -v` | `git@github.com:relux-works/tgfs.git` — unchanged ✓ |
| Apple IDs not invented | cross-read `TASK-260716-1jswke` progress | real, portal-registered ✓ |

The "deliberately NOT changed" justifications (dated `.research/` snapshots, advisory path, DEC-019
rationale, prior-art links) are sound — rewriting dated research would falsify the record that
motivated DEC-019. The prose-not-`PLAT-005` design note is also correct reasoning: minting an ID
without a matrix row + board element would fail the validator.

## Blocking finding — F1: v1 App Group is wrong in the new spec section

**File:** `.spec/platform-requirements.md:15` and `:23` (both added by this task)

### F1a — The table omits the identifier v1 actually ships

`platform-requirements.md:23` lists, under **"Registered Apple identifiers"**:

| `group.com.reluxworks.gramdrive` | App Group shared container |

But `TASK-260716-1jswke` (done, human-executed) established:

> App Group `group.com.reluxworks.gramdrive` registered manually in the portal (**iOS + macOS 15+
> future**); **macOS 14 v1 builds use the team-prefixed group `262RZ595FP.com.reluxworks.gramdrive`
> in entitlements — needs NO portal registration and no provisioning profile with Developer ID.**

And DEC-017 / POL-5 fix the v1 support matrix at **macOS 14 (Sonoma), arm64 only** — macOS 14 is the
*only* v1 platform; iOS and macOS 15+ are explicitly out of scope.

So the table presents the **future/iOS** App Group as *the* App Group, and the identifier that v1
actually ships is absent from the spec entirely.

**Failure scenario:** an implementer picks up `PLAT-MAC-003 (V1)` ("Use an App Group/shared container
for provider metadata and materialized handoff"), reads `platform-requirements.md` as the source of
truth, puts `group.com.reluxworks.gramdrive` in the macOS 14 entitlements, and hits the Developer ID
provisioning-profile wall that 1jswke already diagnosed and solved. That discovery cost a human-gated
portal round-trip; it is now nowhere in the spec.

### F1b — The normative prefix rule contradicts its own table

`platform-requirements.md:15` states:

> Every shipped bundle, package, **App Group**, sync-root, and provider-domain identifier uses the
> `com.reluxworks.gramdrive.*` prefix.

No real App Group identifier can satisfy this as literally written. Apple mandates either a
`group.`-prefixed form (`group.com.reluxworks.gramdrive`, line 23 — contradicting line 15 eight lines
later) or a team-prefixed form (`262RZ595FP.com.reluxworks.gramdrive`). Both carry a mandatory prefix
*before* `com.reluxworks.gramdrive`. The rule needs to admit the Apple-mandated prefix forms rather
than assert something the platform disallows.

## Requested changes (narrow — do not re-do the rollout)

In `.spec/platform-requirements.md` § "Identifier and naming convention (DEC-019 / POL-7)":

1. **Add the v1 macOS App Group to the table** — `262RZ595FP.com.reluxworks.gramdrive`, marked as the
   macOS 14 v1 entitlement form (no portal registration, required for Developer ID signing), with
   `group.com.reluxworks.gramdrive` marked as iOS + macOS 15+ / future. Source: `TASK-260716-1jswke`
   progress notes. Do not invent values.
2. **Reword the line 15 rule** so the App Group clause accommodates Apple's mandatory `group.` /
   team-ID prefixes — e.g. identifiers are *derived from* / *based on* the `com.reluxworks.gramdrive`
   namespace, with App Groups additionally carrying the platform-mandated `group.` or team prefix.
3. Consider a one-line pointer to DEC-017/POL-5 so the macOS-14-vs-15+ split is traceable.

Out of scope for the fix: the rest of the rollout (verified good), the `.research/` survivors, and
the prose-not-`PLAT-005` decision.

## Non-blocking — carried forward, agreed

- POL-7 trademark / handle / domain check before public release — still open, human-only.
- my.telegram.org app title still the legacy `memori` (per `TASK-260716-1iypv4`) — user-visible-adjacent
  surface now inconsistent with GramDrive. Human-only; worth its own board item rather than a note
  that decays.
- No store-listing artifacts exist yet; POL-7 + the new § already bind them once created.

## Not a stop-the-line

This is ordinary, recoverable doc rework against an already-accepted decision (DEC-019/DEC-017) using
facts already established by a completed task. No human decision or external input is needed →
`to-dev`, not `blocked`.
