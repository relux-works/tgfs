# tgfs-ff-lander

Future-only, repository-scoped fast-forward landing core for
`relux-works/tgfs`. The executable accepts only `land --pr N`. Its private
receive-pack adapter binds the final preflight `main` object as the Git wire
`old-id`, emits one update of `refs/heads/main`, and sends an empty pack.

The crate deliberately exposes no generic HTTP/Git write client. Production
wiring supplies typed read capabilities, a private token broker, and the fixed
receive-pack exchange from inside the executable image.

The checked-in binary connects only to the owner-scoped fixed Unix socket
`/tmp/tgfs-ff-lander-bootstrap-<effective-uid>/bootstrap.sock`. The separate
bootstrap broker owns the Keychain credential and returns only opaque one-use
handles; the PAT never crosses the socket. Missing broker, insecure socket
permissions, incomplete reads, or missing signed attestation inputs are closed
refusals. No runtime flag or environment variable can select another socket,
repository, ref, service, request body, or credential.

The owner-authorized bootstrap broker is `.scripts/tgfs_ff_bootstrap_broker.py`.
It is a temporary self-hosting boundary, not the permanent three-App design.
It performs fixed REST/GraphQL pagination, a disposable local SSH verification,
durable create-only audit intent/recovery, Keychain-backed authentication, and
the fixed smart-HTTP exchange. It accepts no arguments and emits no token or
GitHub response content. Install fresh independently signed `policy.json` and
`push-<PR>.json` under its owner-only runtime evidence directory before use.

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
python3 -m unittest discover -s .scripts/tests -p 'test_tgfs_ff_*py'
```

The destructive rehearsal is explicit and separate:

```sh
python3 .scripts/tgfs_ff_rehearsal.py
```

It creates one task-named public repository, proves exact ruleset PUT/GET,
empty-pack success, stale-old refusal, unchanged object set, bypass rule-suite
attribution, and exact ruleset-only rollback. It retains the successful repo
for independent review and prints only privacy-safe identifiers and booleans.
