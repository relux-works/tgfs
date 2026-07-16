# TASK-260717-3dvved — Rework against review verdict F1

Scope: narrow rework only. The naming rollout itself was accepted by review and was **not** touched.
Single file changed: `.spec/platform-requirements.md` § "Identifier and naming convention (DEC-019 / POL-7)".

## F1a — v1 App Group was missing from the table (fixed)

The table listed only `group.com.reluxworks.gramdrive` (the iOS / macOS 15+ future form) under a
"Registered Apple identifiers" heading, so the identifier v1 actually ships was absent from the spec.

Fix: added `262RZ595FP.com.reluxworks.gramdrive` as the entitlement form v1 ships, and marked
`group.com.reluxworks.gramdrive` as iOS + macOS 15+ / future, not used by v1.

Secondary correction the reviewer did not flag: the heading read **"Registered** Apple identifiers",
but the team-prefixed group is deliberately *not* portal-registered — filing it under that heading
would have introduced a fresh contradiction. Heading widened to "Apple identifiers" and a
**Registration** column added, so each row states its own registration status instead of inheriting a
blanket claim from the heading.

## F1b — normative prefix rule contradicted its own table (fixed)

Old: "Every shipped bundle, package, App Group, sync-root, and provider-domain identifier uses the
`com.reluxworks.gramdrive.*` prefix." No real App Group can satisfy this — Apple mandates a `group.`
or team-ID prefix *ahead* of the namespace.

New: identifiers are "derived from the `com.reluxworks.gramdrive` namespace", with an explicit clause
that App Groups additionally carry the platform-mandated `group.` or team-ID prefix.

## DEC-017 / POL-5 pointer (added)

One-line pointer added tying the macOS-14-vs-15+ split to the accepted support matrix, with
`TASK-260716-1jswke` named as the source of the identifier values.

Nuance applied beyond the reviewer's literal wording: the reviewer's report and the 1jswke note both
say "macOS 14 v1 builds". The accepted matrix is macOS **14+**, so a single build must run on 14 and
therefore uses the team-prefixed form throughout. Row labelled "the entitlement form v1 ships (macOS
14 deployment target)" rather than "on macOS 14", which could be misread as implying per-OS-version
builds.

## Values sourced, not invented

All values read from `TASK-260716-1jswke_apple-signing-assets/progress.md` (done, human-executed).
Progress line 29 verbatim:

> Identifier plan (2026-07-17): App Group group.com.reluxworks.gramdrive registered manually in the
> portal (iOS + macOS 15+ future); macOS 14 v1 builds use the team-prefixed group
> 262RZ595FP.com.reluxworks.gramdrive in entitlements — needs NO portal registration and no
> provisioning profile with Developer ID.

Team ID `262RZ595FP` corroborated by lines 24/28/30 (Developer ID Application, Relux Works LLC).

## Verification (re-run after the edit)

| Check | Command | Result |
|---|---|---|
| Traceability validator | `python3 .scripts/validate_traceability.py` | OK 201/201, exit 0 — matches pre-rework baseline |
| `tgfs` as product name | `grep -rniE '\btgfs\b' .spec/ docs/ README.md SECURITY.md` | 8 hits, all legitimate codename/repo-path uses (DEC-019, POL-7, GLOSSARY codename entry, R-015, SECURITY advisory path, README naming para) |
| `tgfiles` | same paths | 1 hit — DEC-019 rationale only (must retain why it was rejected) |
| Wrong-cased gramdrive | grep excluding identifier forms | clean |
| Repo NOT renamed | `git remote -v` | `git@github.com:relux-works/tgfs.git` — unchanged |

## Not addressed (out of scope, carried forward)

- POL-7 trademark / handle / domain check before public release — human-only, still open.
- my.telegram.org app title still legacy `memori` (`TASK-260716-1iypv4`) — human-only; reviewer
  suggested its own board item rather than a decaying note. Not created by this task.
