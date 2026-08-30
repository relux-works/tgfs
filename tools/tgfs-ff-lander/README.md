# tgfs-ff-lander

Future-only, repository-scoped fast-forward landing core for
`relux-works/tgfs`. The executable accepts only `land --pr N`. Its private
receive-pack adapter binds the final preflight `main` object as the Git wire
`old-id`, emits one update of `refs/heads/main`, and sends an empty pack.

The crate deliberately exposes no generic HTTP/Git write client. Production
wiring supplies typed read capabilities, a private token broker, and the fixed
receive-pack exchange from inside the executable image.

The checked-in binary is release-disabled: its sealed backend always refuses
before token minting. Release engineering must replace that private backend
with reviewed, repository-fixed wiring and pinned policy/bypass digests; no
runtime flag or environment variable enables it. This prevents a source build
from accidentally becoming a bypass-capable lander.

## Capability boundaries

| Component | Allowed | Explicitly unavailable |
| --- | --- | --- |
| Lander | Metadata read, PR read, Checks read, Statuses read, broker-minted in-memory Contents token, fixed `git-receive-pack` | Administration, Workflows write, webhook secret, attestation signing keys, evidence writes, generic HTTP/Git client |
| Policy attestor | Complete effective ruleset/history reads and its own KMS signing operation | Contents write, bypass actor, lander token or transport |
| Push attestor | Webhook delivery receive/reconciliation reads, append-only ledger, its own KMS signing operation | Contents write, bypass actor, lander token or transport, policy evidence writes |

The lander pins distinct Ed25519 verification keys. Policy statements bind the
complete ordered ruleset IDs and update times. Push statements bind the GitHub
delivery ID/GUID, durable receive time, terminal page-chain watermark,
retention window, and exact PR/head/candidate/pusher tuple.

## Ownership

Repository delivery/security tooling. It is isolated from the GramDrive core
workspace so delivery credentials and network code cannot enter shipped app
libraries.

## Test command

```sh
cargo test --manifest-path tools/tgfs-ff-lander/Cargo.toml
cargo clippy --manifest-path tools/tgfs-ff-lander/Cargo.toml --all-targets -- -D warnings
```
