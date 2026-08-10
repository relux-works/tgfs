# Contributing

This repository is in specification and decomposition phase. Product implementation must not begin until the task-board plan is approved.

## Source of truth

- Product and engineering requirements: `.spec/`
- Research and evidence: `.research/`
- Open decisions and risks: `docs/`
- Work decomposition and status: `.task-board/`
- Generated phase plans: `.planning/`

## Workflow

1. Create or refine work through `task-board`; do not edit `.task-board/**/progress.md` manually.
2. Reference requirement IDs in task descriptions and acceptance criteria.
3. Update specifications before changing product or architecture scope.
4. Keep research in `.research/{YYMMDD}-{topic}.md` and attach it to its board task as an outcome resource.
5. Use small, conventional commits and keep generated/local runtime state out of Git.
6. Never commit credentials, Telegram sessions, user content, diagnostic archives, or local databases.
7. Contributions are submitted under the [Apache License 2.0](LICENSE). Keep
   copyright, license, and third-party notices intact, and add any required
   notices for new third-party material.

## Current boundary

The board may contain future implementation tasks, but this repository currently authorizes documentation, planning, research, and spikes only when explicitly approved. No product implementation is part of the current setup/decomposition request.

## Reporting security issues

Do not use an issue or pull request for a vulnerability report. Follow
[SECURITY.md](SECURITY.md) and use a private GitHub Security Advisory instead.
