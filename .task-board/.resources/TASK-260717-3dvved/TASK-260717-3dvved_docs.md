# TASK-260717-3dvved — Apply public name GramDrive (DEC-019 / POL-7)

Date: 2026-07-17
Role: doc-writer (docs only; no code, no board files edited by hand)

## Scope applied

Public product name **GramDrive** applied across user-visible naming; repository/codename `tgfs`
deliberately retained and **not** renamed (verified: `origin` is still `git@github.com:relux-works/tgfs.git`).

## Files changed

| File | Change |
|---|---|
| `README.md` | Title `# TGFS — Telegram File System` → `# GramDrive`. Naming paragraph now cites DEC-019 + POL-7, states the identifier prefix, that `tgfs` must not appear in user-visible strings/marketing/store listings, and that the repo is deliberately not renamed. |
| `.spec/README.md` | Index title → `GramDrive Specification Index`. |
| `.spec/product.md` | Product statement and journey J5 → GramDrive. |
| `.spec/domain-model.md` | Canonical ownership → "GramDrive structured metadata". |
| `.spec/sync-and-filesystem-semantics.md` | SYNC-053 text → "GramDrive cache state" (requirement ID unchanged). |
| `.spec/platform-requirements.md` | **New** § "Identifier and naming convention (DEC-019 / POL-7)" under Shared rules — prefix rule, user-visible-surface rule, registered Apple identifier table, Android/Windows/Linux derivation. |
| `.spec/architecture.md` | Pointer under "Native layers" to the identifier convention. |
| `docs/GLOSSARY.md` | Added **GramDrive** (public name) and **tgfs** (internal codename only) entries. |
| `docs/RISK_REGISTER.md` | R-015 remediated: was "Decide public product/repository naming before external launch; current private repo name is provisional" (stale — decision already made). Now records DEC-019/POL-7 mitigation + residual trademark/handle check. |
| `SECURITY.md` | "TGFS is pre-implementation" → "GramDrive (internal repository codename `tgfs`) …". |
| Board `STORY-260715-31thz2` | Description "Expose TGFS roots…" → "Expose GramDrive roots…" via `task-board m set_details` (CLI, not a manual file edit). |

`Last updated:` bumped 2026-07-15 → 2026-07-17 on every spec/doc changed, matching the convention
`policies.md` set.

## Identifier convention recorded

Prefix `com.reluxworks.gramdrive.*` on every shipped bundle, package, App Group, sync-root, and
provider-domain identifier. Concrete Apple values are sourced from TASK-260716-1jswke progress
(registered with the portal 2026-07-17), not invented:

| Identifier | Use |
|---|---|
| `com.reluxworks.gramdrive` | Containing application (macOS, iOS) |
| `com.reluxworks.gramdrive.fileprovider` | File Provider extension |
| `group.com.reluxworks.gramdrive` | App Group shared container |

## Design decision: prose, not a new `PLAT-*` requirement ID

The identifier convention was recorded as **prose under a `###` heading**, deliberately not as a new
`PLAT-005` bullet. Reason: `.scripts/validate_traceability.py` registers a requirement ID from the
bullet form `- **PLAT-00N (V1):**` and then *fails* unless that ID has exactly one row in
`docs/TRACEABILITY.md` mapped to a real board element. Minting an ID would have broken the validator
or forced an invented board mapping. Prose carries the same normative content with no ID debt.
If this convention should become a testable requirement, it needs a real board element + matrix row.

## Verification evidence

```
=== VALIDATOR ===
OK: 201 requirements from .spec/ all mapped exactly once (166 active, 24 deferred-platform,
10 deferred-optional, 1 future); 125 board elements referenced; no orphan references on the board.

=== A. TGFS used as product name in .spec/, docs/, README, SECURITY ===  clean
=== B. tgfiles outside DEC-019 rationale ===                             clean
=== C. wrong-cased GramDrive ===                                         clean
=== D. TGFS on board ===                                                 clean
=== E. repo NOT renamed ===  origin git@github.com:relux-works/tgfs.git / tgfs
```

Baseline before edits was also `OK: 201 requirements` — no regression.

## Deliberately NOT changed (justified `tgfs` survivors)

- `.research/*` — permanent research archive, dated 260715 snapshots predating DEC-019 (260717).
  These use `tgfs` as the then-current working name and document the `TheodoreKrypton/tgfs` prior-art
  collision. Rewriting dated research would falsify the historical record that *motivated* DEC-019.
- `SECURITY.md:5` `relux-works/tgfs` — the actual GitHub Security Advisories path; must stay real.
- `docs/RISK_REGISTER.md` R-015 / `README.md` — `TheodoreKrypton/tgfs` prior-art links.
- `.spec/decisions.md` DEC-019 — rationale legitimately names both rejected candidates (`tgfs`,
  `tgfiles`). This is the decision record and must retain why they were rejected.
- `.spec/policies.md` POL-7 — already correct; states repo/codename may remain `tgfs`.
- Repository name, git remote, `docs/OPEN_QUESTIONS.md:36` (already correctly resolved to DEC-019).

## Follow-ups for the owner (not blocking this task)

1. **Trademark / handle / domain check before public release** — required by POL-7, still open.
2. **Telegram app title is stale.** Per TASK-260716-1iypv4 progress, the registered app title on
   my.telegram.org is the legacy name `memori`. That is a user-visible-adjacent surface now
   inconsistent with GramDrive. Human-only (my.telegram.org login); noted, not blocking.
3. **No store-listing artifacts exist yet** to apply the name to — POL-7 + the new
   platform-requirements § already bind them to GramDrive when they are created.
