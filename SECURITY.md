# Security Policy

GramDrive (internal repository codename `tgfs`) is pre-implementation and has no released security-supported version yet.

Do not open public issues containing Telegram credentials, session data, phone
numbers, message content, file names, diagnostic archives, or other user data.
Report suspected vulnerabilities privately through [GitHub Security
Advisories](https://github.com/relux-works/tgfs/security/advisories/new).

Include a minimal, sanitized reproduction, the affected revision, impact, and
any suggested mitigation. Do not attach live credentials or customer data. The
maintainers will acknowledge a report privately and coordinate disclosure only
after a fix or mitigation is available.

The repository's public-release requirements, including Actions and release
surface review, are in [docs/PUBLIC_REPOSITORY_READINESS.md](docs/PUBLIC_REPOSITORY_READINESS.md).

The security requirements for future implementation are defined in [`.spec/security-and-privacy.md`](.spec/security-and-privacy.md). In particular, source code and task-board resources must never contain real Telegram authorization data or user content.
