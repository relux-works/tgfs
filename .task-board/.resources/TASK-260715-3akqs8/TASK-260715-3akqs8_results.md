# TASK-260715-3akqs8 — Package shared core for native consumers

Implementation notes, revision 2 (rework per `TASK-260715-3akqs8_review-verdict.md`).
Status: ready for review.

## What changed in this rework

Three blockers and one nit from the review. B1 turned out to have a different
answer than any of the three options offered, because the review's root cause
does not survive re-measurement — details below. B2 and B3 are fixed as asked.

| # | Finding | Resolution |
|---|---|---|
| B1 | `path_independent: true` is false; ThinLTO hashes module paths | **Re-measured: the claim is true; the path is a non-variable.** The real variable is target-dir reuse. Fixed at the source: the shipped library builds in a wiped, dedicated target dir. Field is now computed from the build, not a literal |
| B2 | The check can't observe path-independence | `--check-reproducible` now stages the source to two different paths and builds each from clean |
| B3 | `.build/` would be committed; `copytree` has no `ignore=` | gitignored, removed, `ignore=` added with three tests |
| NIT | Space-separated `RUSTFLAGS` | `CARGO_ENCODED_RUSTFLAGS`, with a path-containing-a-space test |

## B1 — the review's numbers are right; the attribution is not

The review measured `bab48d50` at the main checkout and `110b1b9a` elsewhere and
concluded the checkout path drives the bytes, via ThinLTO's `.llvm.<hash>`
suffixes. I reproduced both numbers exactly, then varied one axis at a time.

**Varying the path alone (`.temp/TASK-260715-3akqs8/repro-experiment.sh`):**

| Build of identical source | sha256 |
|---|---|
| staged copy at `…/repro/a` | `110b1b9a…` |
| staged copy at `…/repro/b-considerably-longer-path-name` | `110b1b9a…` |
| staged to canonical `/private/tmp/gramdrive-core-build` | `110b1b9a…` |
| canonical path, **different `CARGO_HOME`** (copied registry) | `110b1b9a…` |

Three paths of different lengths, two target-dir paths, two `CARGO_HOME`s: one
value. The path is a measured non-variable.

**Varying the target dir alone, at the main checkout (`repro-q4.sh`):**

| Build at the *same* path | sha256 | size |
|---|---|---|
| existing `target/`, `cargo clean -p` first (what the old check did) | `bab48d50…` | 7,920,568 B |
| **fresh target dir** | `110b1b9a…` | 7,920,584 B |

Same path, same source, two target states, two stable values. `bab48d50` is the
review's main-checkout number; `110b1b9a` is the clean-build value at *every*
path including that one.

**So the review's experiment confounded two variables**: its "main checkout" arm
reused a polluted `target/` (547 dependency artifacts, an `incremental/` dir, a
stale rlib and dylib from ordinary `cargo build` runs) while its "other path" arm
was a fresh checkout with a fresh target. The symptom was real and reproducible;
the cause was the target dir, not the path.

The review's LLVM analysis holds as far as the symptom goes — I confirmed the
sole delta is `.llvm.<hash>` local-symbol suffixes: 399 archive members with
identical names, symbol count identical at 3896, and rustc's own mangled hashes
(`…17hfa3520658262392cE`) byte-identical between the two. But
`--remap-path-prefix` failing to reach LLVM module IDs is not what produced it
here, since moving the path never moved the bytes.

**Fix.** LTO stays and `path_independent: true` stays, because it is true — but
it is now true *by construction* rather than by luck: the shipped library builds
with `CARGO_TARGET_DIR=.temp/packaging/target`, wiped before every build, so the
artifact cannot inherit whatever a developer's `target/` accumulated. `cargo
clean -p` is not an alternative — it drops the named crate and keeps every
dependency, which is exactly the state that moves the bytes. The bindings
generator deliberately stays on the repo's `target/`: it is a host tool, nothing
of it ships, and keeping its `--features bindgen` build out of the packaging
target dir is what keeps that dir single-purpose.

The review's structural point stands and is fixed: `path_independent` is no
longer a literal. It is computed from a `BuildRecord` of what the build actually
did (`clean_target_dir and remapped_to`), so a future change that reuses a target
dir or drops the remapping flips the field instead of lying to consumers. A
manifest for a build that never ran reports `false`, not an omitted field.

## B2 — the check now varies the axis the claim is about

`--check-reproducible` stages `BUILD_INPUTS` to two paths (`a` and
`b-a-considerably-longer-checkout-directory`), builds each from its own clean
target dir, and compares. Both builds go through the same `build_env` +
`cargo_staticlib_argv` + clean-target-dir procedure the shipped artifact does —
a check whose procedure differs from the shipped build's covers some other build
that happens to live in the same file.

The tie is now visible end to end: the check reports `110b1b9a…`, and that is
the digest of the library inside the shipped XCFramework. Before, the check
reported `bab48d50` and the artifact was whatever the dev's target dir produced.

## B3 — hygiene

- `.gitignore`: `.build/` repo-wide (build output anywhere), and
  `Package.resolved` scoped to the verifier package only — it resolves a single
  path dependency on a freshly staged artifact, so a committed resolution pins
  nothing. A future shipped app package should commit its own, which a repo-wide
  ignore would have silently prevented.
- Removed the untracked `.build/`. Confirmed: `git add -An .scripts/packaging/`
  now stages exactly the four source files, no `data.mdb`/`lock.mdb`.
- `copytree(..., ignore=shutil.ignore_patterns(*COPY_EXCLUDES))`, covered by
  `PrepareConsumerTest` (copies sources / excludes stale SwiftPM state and leaves
  the source untouched / a rerun replaces the previous copy).

## NIT — encoded rustflags

`remap_rustflags` returns a list; `build_env` joins it into
`CARGO_ENCODED_RUSTFLAGS` and drops `RUSTFLAGS` rather than leaving a value that
cargo ignores sitting next to the one it reads. `caller_rustflags` honors
cargo's own precedence (encoded wins). Tested with a repo path containing a
space — the case that would otherwise split the remap into two broken flags and
leave the build succeeding with reproducibility quietly gone.

## One defect found and fixed during the rework

Recording the remapping in the manifest initially wrote the `<from>=<to>` pairs,
which put `/Users/iv/Developer/ReluxWorks/tgfs` and `/Users/iv/.cargo` into
`gramdrive-core-manifest.json` — a file that ships inside the zip. That undoes in
JSON exactly what the remapping strips out of the binary, and it would have
broken the review's own verified property ("zero `/Users/...` strings in the
shipped archive"). The field now records only the destinations
(`path_prefixes_remapped_to: ["/gramdrive", "/cargo"]`), which is also the more
useful half for anyone reproducing the artifact. Two tests pin it, one of them
asserting the whole shipped manifest carries no local build path.

Verified on the real artifact: no `/Users/` string anywhere in
`.temp/packaging/GramDriveCore/`, nor in any member of the zip.

## Verification run (all re-run for this rework)

| Command | Result |
|---|---|
| `make check` | 8/8 gate steps passed |
| `python3 -m unittest discover -s .scripts/tests` | **113 tests OK** (87 → 113) |
| `make package` | PACKAGING PASSED; Swift consumer ran `probe_transfer`: 100 bytes, progress `[40, 80, 100]` |
| `make package-reproducible` | REPRODUCIBLE at 2 paths: `110b1b9a…` (27s) |
| shipped library digest == checked digest | `110b1b9a…` — the check covers the artifact |
| two full `make package` runs | identical zip `a3e976af…` |
| `make smoke-bindings` | BINDINGS SMOKE PASSED (unaffected) |
| `/Users/` strings in artifact and zip | none |

Sizes (0.1.0, macos-arm64, re-measured — the clean build is 16 B larger than the
polluted one the previous revision measured): staticlib 7,920,584 B; XCFramework
7,948,400 B; artifact 8,014,481 B; zip 2,260,638 B. `strip -S` → 2,695,152 B, so
the debug-info figure and the decision to ship it are unchanged.

Manifest still records `worktree_clean: false` / `describe: e60603d-dirty`
because these changes are uncommitted — correct behavior, not a defect.

## Unchanged from revision 1

The architecture, pipeline shape, Swift consumer proof, per-platform matrix
(Windows/Linux consume the crate; Android/iOS deferred not stubbed), the
crate-type override that restores LTO, and the ship-debug-info decision all
survive the rework as the review expected. `Cargo.toml`'s CAVEAT → SETTLED edit
stands: LTO was never the problem.

## Evidence

- `.temp/TASK-260715-3akqs8/repro-experiment.sh` + `.log` — path, canonical-path
  and `CARGO_HOME` axes
- `.temp/TASK-260715-3akqs8/repro-q4.sh` + `.log` — the decisive target-dir axis
- LOGBOOK 0552 (supersedes 0542) and 0553

## Note for review

The one thing I'd most like checked is B1's re-measurement, since it contradicts
a review finding rather than implementing it. `repro-q4.sh` is the whole argument
and runs in ~15s: same path, two target states, two digests. If that reproduces,
the path attribution is wrong and the fix is aimed at the right variable. If it
does not reproduce on another machine, I want to know before this ships.
