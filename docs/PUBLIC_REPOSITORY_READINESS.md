# Public repository readiness

This document is the release-manager checklist for converting
`relux-works/tgfs` from private to public. The visibility change is irreversible
for exposed history, releases, Actions logs, and URLs. Do not perform it until a
reviewer has accepted the accompanying audit evidence.

## Audit record — 2026-08-10

| Surface | Evidence | Result |
|---|---|---|
| Pushed refs and history | GitHub refs: `main`, `v0.1.0`, `v0.1.1`; all resolve locally. `gitleaks detect --log-opts=--all --redact` scanned 75 commits / about 7.22 MB. | No findings. |
| Repository objects | `git fsck --no-reflogs --full --strict` completed. Dangling local objects are not reachable from a remote ref. | No pushed-object integrity error. |
| Release downloads | All 16 assets from the two published legacy releases were downloaded and scanned with redaction (about 12.48 MB). | No findings. |
| Actions artifacts | GitHub listed retained artifacts as expired. | Nothing remains downloadable as an artifact. |
| Actions logs | The reviewer-attested orientation audit is preserved as task-scoped resources: `TASK-260810-22vz0s_prior-all-actions-gitleaks-report.json` (empty) and `TASK-260810-22vz0s_prior-all-actions-gitleaks-run.log` (56 retained logs, about 2.30 MB, no leaks). A repeat scan retrieved and scanned 42 logs (about 1.63 MB) with no findings; 14 older-run logs could not be re-downloaded because the GitHub log blob host was unavailable from this environment. | The accepted orientation evidence covers all 56 retained logs. Keep both task-scoped resources with the publication record; if their attribution is lost, re-run the log scan from a network that can reach GitHub's log host before visibility changes. |
| Repository configuration | The repository is private; Pages is unconfigured; Actions defaults to read-only tokens and cannot approve PR reviews. GitHub currently reports secret scanning and Dependabot alerts disabled. | Configure the public posture below after the visibility change. |

This audit lists secret names only, never values: `APPSTORE_*`,
`MACOS_CERT_*`, and `TELEGRAM_API_*`. Do not move values into repository files,
issues, workflow logs, artifacts, or release notes.

## Legacy release disposition

`v0.1.0` and `v0.1.1` are private-era, published legacy releases. They are not
public-launch candidates: both predate the Apache-2.0 notice, the public
community metadata, and the public-posture review.

Before the repository becomes public, a release manager must convert both
published releases to GitHub drafts. Preserve both releases, their tags, and
their assets; do not delete, archive, or remove any of them. Do not merely mark
either release as latest, and do not reference either version in a Sparkle
appcast. There is no
Pages site or Sparkle feed configured at this revision; the first public feed
must begin with a reviewer-approved post-audit release and must explicitly
reject `v0.1.0` and `v0.1.1`.

Verify that the draft releases, while their tags and assets remain retained,
cannot be accessed by unauthenticated users after the repository becomes
public. This reversible draft-only disposition is the accepted
release-management action for this checklist.

## Public-switch sequence

1. Review this document and its task-scoped audit evidence. Confirm the
   reviewer-attested all-56-log orientation evidence remains attributable,
   confirm no unreviewed ref, fork, or release surface exists, and retry the
   inaccessible downloads only if that evidence is unavailable.
2. Convert the two legacy releases to GitHub drafts, preserving their tags and
   assets. Verify no Sparkle feed contains either version and record the draft
   state before continuing.
3. Confirm `LICENSE`, `NOTICE`, `SECURITY.md`, `CONTRIBUTING.md`,
   `CODE_OF_CONDUCT.md`, issue forms, and `CODEOWNERS` are merged on `main`.
4. With reviewer approval, change visibility in GitHub. Then verify from an
   unauthenticated session: clone, archive download, releases, issues, Actions
   runs/logs, security policy, and Pages (only if deliberately enabled).
5. Immediately create the `main` ruleset: require pull requests and code-owner
   review, require the `rust-core` and `secret-scan` checks, block force pushes
   and branch deletion, and apply it to administrators as well as contributors.
   Confirm GitHub accepts `.github/CODEOWNERS` and replace its owner only when
   ownership changes.
6. Enable secret scanning, push protection, and Dependabot alerts. Keep Actions
   default permissions read-only; do not allow pull requests to approve
   workflows. Limit the release environment to required reviewers and scope
   its secrets to the release job only.
7. Keep Pages disabled until the stable Sparkle publisher exists. When enabling
   it, grant `pages: write` and `id-token: write` only to that approval-gated
   publisher; never grant them to test or ordinary CI workflows.

Record the anonymous verification results and the resulting ruleset identifiers
in the task outcome before treating public publication as accepted.
