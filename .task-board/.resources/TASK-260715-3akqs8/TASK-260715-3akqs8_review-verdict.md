# TASK-260715-3akqs8 — Review verdict: CHANGES REQUESTED (→ to-dev)

Reviewed by independent re-execution, not by reading the results doc. Most of
this task is genuinely good work and the numbers in the results doc are honest —
I reproduced them to the byte. One claim is false, and it is the central one.

## What I verified as PASSING

| Check | Result |
|---|---|
| `make check` | 8/8 gate steps passed (exit 0) |
| `python3 -m unittest discover -s .scripts/tests` | 87 tests OK |
| `make package` | PACKAGING PASSED, end to end |
| Swift consumer runs `probe_transfer` on macOS arm64 | 100 bytes, progress `[40, 80, 100]` — **real** |
| Version-identifiable | contract `0.1.0` read from the built binary by running it; commit/toolchain/uniffi 0.32.0 recorded |
| Size measured | staticlib 7,920,568 B; xcframework 7,948,384 B; artifact 8,014,081 B; zip 2,260,468 B — all match the doc exactly |
| Time-independence of the zip | my run at a different wall-clock produced the **identical** zip sha `fb36923d…` — genuinely reproducible over time |
| Path remapping works for debug info | zero `/Users/...` strings in the shipped archive |

The `--crate-type staticlib` → LTO reasoning, the debug-info-stays argument, the
"documented, no artifact" call for Windows/Linux, and the deferral of Android/iOS
are all sound and well-argued. The Swift consumer is a real SwiftPM path
dependency, not a rigged compiler invocation. Credit where due.

## FINDING 1 (blocking) — `path_independent: true` is false for the shipped build

`gramdrive-core-manifest.json` asserts, unconditionally:

    "path_independent": true,
    "note": "... the same commit produces the same bytes at any path and any time."

It does not. Measured, byte-identical source (`crates/`, `Cargo.toml`,
`Cargo.lock`, `rust-toolchain.toml` all verified identical via `diff`), only the
checkout path varying:

| Build | main checkout | different path | |
|---|---|---|---|
| plain (no LTO) | `3a58076b…` | `3a58076b…` | path-independent |
| **shipped** (`--crate-type staticlib` → ThinLTO) | `bab48d50…` | `110b1b9a…` | **DIFFERS** |

Deterministic, not flaky: rebuilding from clean at the second path reproduced
`110b1b9a…` exactly. Two paths, two stable-but-different outputs.

**Root cause.** The only delta between the two archives is LLVM's
`.llvm.<hash>` local-symbol suffixes (399 archive members, identical names;
`strings` diff shows only these). ThinLTO derives those suffixes from the module
identifier, which is the on-disk bitcode path. `--remap-path-prefix` rewrites
**debug info only** — it does not touch LLVM module identifiers. So the
`--crate-type staticlib` override that this task introduced to restore
`lto = "thin"` is exactly what broke path-independence. The two settled
decisions interact, and that interaction was not measured.

This directly contradicts:
- the manifest note quoted above (which ships to consumers);
- `remap_rustflags`'s docstring: "with both prefixes remapped both produced
  bab48d50" — the second half is not reproducible;
- the results doc's table row "worktree at another path, remapped → `bab48d50`".

`bab48d50…` is the LTO build **at the main checkout path**; I reproduced it
there. A build at any other path is not that value. The worktree row looks like
it was measured before the crate-type override landed, or read back from the
main checkout's `target/`, and was not re-measured afterwards.

## FINDING 2 (blocking, and why Finding 1 survived) — the check can't catch it

`check_reproducible()` runs `cargo clean -p` + build **twice at the same path**.
That tests determinism and time-independence — both of which genuinely hold —
but it structurally cannot observe path-independence, which is the property the
remapping exists for and the only one the manifest actually asserts. The
docstring says "the claim is worth exactly as much as the check behind it";
correct, and here the check is weaker than the claim. A check that built at two
different paths would have failed on day one.

## FINDING 3 (blocking, hygiene) — build output will be committed

`.scripts/packaging/swift-consumer/.build/` (60K, incl. binary SwiftPM index
`data.mdb` / `lock.mdb`) exists in the source tree and is **not** gitignored:

    $ git check-ignore -v .scripts/packaging/swift-consumer/.build  ->  NOT IGNORED
    $ git add -An .scripts/packaging/
    add '.scripts/packaging/swift-consumer/.build/index-build/.../data.mdb'
    add '.scripts/packaging/swift-consumer/.build/index-build/.../lock.mdb'

The next `git add .scripts/packaging/` commits SwiftPM index databases. This
violates the invariant `prepare_consumer`'s own docstring states (".scripts/ is
source"). Secondary: `shutil.copytree(source, consumer_dir)` has no `ignore=`,
so this stale `.build/` is copied verbatim into `.temp/packaging/consumer/` on
every run — verified present after `make package`. The copy protects `.scripts/`
from the build but not the build from `.scripts/`, and carrying stale SwiftPM
state into the package that is meant to be the acceptance test is a false-pass
risk. `prepare_consumer` has no test.

## FINDING 4 (nit, non-blocking)

`remap_rustflags` joins flags into a space-separated `RUSTFLAGS`. A repo path
containing a space would split into broken flags and silently lose the remap.
`CARGO_ENCODED_RUSTFLAGS` (`\x1f`-separated) is the robust form. Latent only —
the current path has no spaces.

## What I am NOT asking for

Not asking to rip out LTO. LTO vs path-independent reproducibility is a real
tradeoff and it is an implementation decision inside the existing architecture —
the implementer's call, not a human-only escalation. Any of these closes it:

1. **Keep LTO, tell the truth.** Make `path_independent` a *measured* field, not
   a literal, and state in the manifest/README that byte-identity holds per build
   path (and over time) but not across paths, because ThinLTO hashes module
   paths. Cheapest, honest, keeps the LTO win.
2. **Keep LTO, get real path-independence.** Build from a canonical fixed path
   (stage/copy the source to a constant location before building) — the standard
   reproducible-builds move, and it makes `path_independent: true` true.
3. **Drop the LTO override**, reverting Cargo.toml's CAVEAT→SETTLED edit and
   accepting no-LTO for the shipped artifact, which is path-independent today
   (`3a58076b…` at both paths).

Whichever is chosen, **`--check-reproducible` must build at two different paths**,
so the check covers the claim. Plus: gitignore `.build/` (and `Package.resolved`),
remove the committed one, and give `copytree` an `ignore=` with a test.

## Verdict

→ `to-dev`. Findings 1–3 are concrete, reproducible, and mechanical to fix. The
architecture, the pipeline shape, and the Swift consumer proof are all sound and
should survive the rework unchanged.
