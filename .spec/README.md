# TGFS Specification Index

Status: planning baseline
Last updated: 2026-07-15

The `.spec/` directory is the source of truth for product and engineering requirements. Research explains why decisions were made; specifications define what future implementation must satisfy. Task-board elements must reference requirement IDs from these files where applicable.

## Documents

| Document | Purpose |
|---|---|
| [product.md](product.md) | Product goal, users, scope, journeys, and functional requirements |
| [architecture.md](architecture.md) | Shared-core architecture and local/remote source strategy |
| [domain-model.md](domain-model.md) | Canonical entities, identities, versions, and relationships |
| [sync-and-filesystem-semantics.md](sync-and-filesystem-semantics.md) | Ordering, rendering, hydration, cache, edits, deletes, and provider behavior |
| [platform-requirements.md](platform-requirements.md) | Native requirements for Apple, Windows, Android, and Linux |
| [security-and-privacy.md](security-and-privacy.md) | Credentials, local data, logging, abuse controls, and privacy boundaries |
| [quality-and-release.md](quality-and-release.md) | Test strategy, non-functional requirements, and release gates |
| [decisions.md](decisions.md) | Accepted and provisional architecture decisions |

Supporting material:

- [../docs/OPEN_QUESTIONS.md](../docs/OPEN_QUESTIONS.md)
- [../docs/RISK_REGISTER.md](../docs/RISK_REGISTER.md)
- [../docs/GLOSSARY.md](../docs/GLOSSARY.md)
- [../.research/260715-telegram-filesystem-landscape.md](../.research/260715-telegram-filesystem-landscape.md)

## Requirement conventions

- `PRD-*`: product and functional requirements
- `DOM-*`: domain-model invariants
- `SYNC-*`: synchronization and filesystem semantics
- `PLAT-*`: platform requirements
- `SEC-*`: security and privacy requirements
- `NFR-*`: non-functional requirements and release gates
- `DEC-*`: architecture decisions

Requirements marked **V1** are committed scope. **Optional tier** requirements are decomposed but do not block the local-first release unless a platform decision promotes them. **Future** requirements are recorded to protect interfaces from obvious dead ends but are not implementation commitments.

## Change control

1. Update the relevant specification before changing scope or architecture.
2. Record material architecture changes in `decisions.md`.
3. Update affected task-board acceptance criteria and dependencies through `task-board`; never edit board progress files manually.
4. Keep unresolved human/product decisions in `docs/OPEN_QUESTIONS.md` and corresponding board decision tasks.
