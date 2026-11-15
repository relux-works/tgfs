# Unresolved questions

## TASK-261115-esaarl: dependency remediation after hosted CI starts

Hosted CI run [43697525097](https://github.com/relux-works/tgfs/actions/runs/43697525097)
ran both required jobs. `secret-scan` passed; `rust-core` passed seven of eight
acceptance gates and failed only `cargo deny check` (exit 1).

The unchanged lockfile contains `libsqlite3-sys 0.37.0`
([RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html)) and
`tokio 1.52.4`
([RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386.html)).
The advisories identify patched ranges `libsqlite3-sys >=0.39.0` and
`tokio >=1.51.6,<1.52.0` or `>=1.53.3`.

The remaining scope decision is who owns a separate dependency-remediation
change and its compatibility validation (including the rusqlite dependency
chain). This producer task migrates only the CI runners and preserves every
gate; it does not update product dependencies or suppress advisories. Keeping
the gates intact leaves the PR blocked until remediation is accepted. The
native/candidate/release runner migrations also remain outside this task.
