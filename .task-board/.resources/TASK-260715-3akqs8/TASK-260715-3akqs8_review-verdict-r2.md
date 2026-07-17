# TASK-260715-3akqs8 — Review verdict, revision 2: ACCEPTED (→ done)

Reviewed by independent re-execution and independent re-measurement, not by
reading the results doc. The rework resolves all three blockers and the nit.

The central question was B1, where the implementer **contradicted** the prior
review rather than implementing one of its three offered options, and explicitly
asked for that re-measurement to be checked before shipping. I checked it. **The
implementer is right and the prior review (0542) was wrong.** Details below,
because a review that overturns a review should show its work.

## B1 — re-measured independently: the path is not the variable

The prior review measured `bab48d50…` at the main checkout and `110b1b9a…`
elsewhere and attributed the difference to the checkout path, via ThinLTO's
`.llvm.<hash>` suffixes deriving from unremapped module IDs. I re-ran the
decisive experiment myself, driving the pipeline's **own** `build_env()` and
`cargo_staticlib_argv()` (so I measured what actually ships, not a hand-rolled
approximation):

| # | Build of identical source | sha256 | size |
|---|---|---|---|
| X1 | **main checkout** — the exact path the prior review measured `bab48d50` at — fresh target dir | `110b1b9a…` | 7,920,584 B |
| X2 | main checkout, fresh target dir at a *different, longer* target-dir path | `110b1b9a…` | 7,920,584 B |
| — | `--check-reproducible`, two staged paths of different lengths | `110b1b9a…` | — |
| — | library inside the shipped XCFramework | `110b1b9a…` | 7,920,584 B |

X1 is decisive and refutes the prior review directly: **at the very path the
prior review said produces `bab48d50`, a clean build produces `110b1b9a` — the
same bytes as every other path.** The path is a measured non-variable. So is the
target-dir path (X2), which the implementer's own `repro-q4` had left
confounded.

### The other half of the causal claim, and a wrinkle worth recording

I also tried to reproduce the *polluted* case at the main checkout. This took
three attempts and the failures are worth writing down:

* Attempt 1 (`cargo clean -p` + build, i.e. exactly what the old check did):
  finished in **0.36s** and reported `bab48d50…`. Too fast to be a build.
  `cargo clean -p` without `--target` does not clean the cross-target
  directory — it hashed a leftover artifact.
* Attempt 2 (delete the `.a`, rebuild): finished in 0.03s, `bab48d50…` again,
  `Compiling` absent. Cargo keeps the real artifact in `deps/` and hardlinks
  ("uplifts") it into `release/`, so deleting the uplifted copy just re-uplifts.
* Attempt 3 (`cargo clean -p gramdrive-ffi --target <triple> --release`, which
  evicts it from `deps/` while leaving the other 468 dep artifacts,
  `incremental/` and the stale rlib/dylib in place, then rebuild — `Compiling`
  confirmed present):

| Build at the **same path** | sha256 | size |
|---|---|---|
| fresh target dir (X1) | `110b1b9a…` | 7,920,584 B |
| polluted target dir, genuine rebuild (X3) | **`a098d5f5…`** | 7,920,568 B |

Same path, two target states, two digests → **target-dir reuse moves the bytes.
Confirmed.**

Note X3 is a *third* value — neither `110b1b9a` nor the prior review's
`bab48d50`. The pollution state differed from when `bab48d50` was measured (my
earlier runs and `make check` churned `target/`). So a reused target directory
does not merely produce "a different stable value" as the docs frame it; it
produces an **unstable** one, while a clean target directory is stable at
`110b1b9a` across every path, target-dir path and `CARGO_HOME` tried. That makes
the chosen fix *more* justified than the write-up claims, not less. The two
polluted values share a size (7,920,568 B) distinct from the clean build's
(7,920,584 B) — a coherent signature.

**Conclusion:** the fix (a dedicated, wiped `.temp/packaging/target`) is aimed at
the real variable. `path_independent: true` is true for the shipped procedure,
and is now derived from a `BuildRecord` of what the build did rather than a
literal, with `--check-reproducible` as the falsifier. The implementer was right
to push back with evidence instead of complying with a wrong instruction.

## B2 — verified fixed

`--check-reproducible` stages `BUILD_INPUTS` to two paths of deliberately
different length and builds each from its own clean target dir. Re-ran it:
`REPRODUCIBLE at 2 paths: 110b1b9a…` in 27s. Both builds go through the same
`build_env` + `cargo_staticlib_argv` + clean-target-dir procedure the shipped
artifact does, and `CheckReproducibleTest` pins the properties that matter (two
*different* paths; a path-dependent build fails; each path gets its own target
dir; a build that writes nothing is not a pass).

**The tie is real and I verified it end to end**: the digest the check reports is
byte-identical to the library inside the shipped XCFramework. Before the rework
the check reported a value the artifact did not have.

## B3 — verified fixed

* `.build/` is ignored repo-wide; `Package.resolved` scoped to the verifier
  package, with the reasoning recorded (a resolution of a single path dependency
  on a freshly staged artifact pins nothing; a future shipped app package should
  commit its own, which a repo-wide ignore would have silently prevented). Sound.
* Tested with a real `.build/index-build/data.mdb` present:
  `git check-ignore` matches, and `git add -An .scripts/packaging/` stages
  exactly the four source files. The committed `.build/` is gone.
* `copytree(..., ignore=shutil.ignore_patterns(*COPY_EXCLUDES))`, covered by
  three `PrepareConsumerTest` cases.

## NIT — verified fixed

`CARGO_ENCODED_RUSTFLAGS` (`\x1f`-separated), `RUSTFLAGS` popped rather than left
unread beside it, `caller_rustflags` honors cargo's real precedence (encoded
wins), tested with a repo path containing a space.

## Self-caught defect — credit

The implementer found and fixed a defect the prior review missed: recording the
remapping as `<from>=<to>` pairs would have written the local checkout and
`CARGO_HOME` into a manifest that **ships inside the zip** — undoing in JSON
exactly what the remapping strips from the binary. Now only destinations
(`/gramdrive`, `/cargo`) are recorded. Verified: **zero `/Users/` strings** in
the artifact tree and in all 8 zip members.

## Verified by re-execution

| Check | Result |
|---|---|
| `make check` | 8/8 gate steps passed |
| `python3 -m unittest discover -s .scripts/tests` | **113 tests OK** (87 → 113) |
| `make package` | PACKAGING PASSED; Swift consumer ran `probe_transfer`: 100 B, progress `[40, 80, 100]`, contract `0.1.0` |
| `make package-reproducible` | REPRODUCIBLE at 2 paths: `110b1b9a…` |
| shipped library digest == checked digest | `110b1b9a…` — the check covers the artifact |
| two full `make package` runs | identical zip `619a82df…` (2,260,634 B) |
| `make smoke-bindings` | BINDINGS SMOKE PASSED (unaffected) |
| `/Users/` in artifact / zip | none, 8 members |
| shipped README size claims | exact: 7,920,584 B → 2,695,152 B after `strip -S` |
| manifest identity | contract `0.1.0` **read from the binary by running it**, crate `0.1.0`, `e60603d-dirty`, `worktree_clean: false`, uniffi `0.32.0` |

`worktree_clean: false` / `describe: e60603d-dirty` are correct — the work is
uncommitted.

## Finding (non-blocking, doc accuracy) — a cited zip checksum that does not reproduce

`.scripts/packaging/README.md:144` states:

> Two full `make package` runs from scratch produce an identical
> `GramDriveCore-0.1.0.zip` (`a3e976af…`).

The **property is true** — I verified two full runs are byte-identical. The
**cited value is not**: I measure `619a82df…` (2,260,634 B), stably, twice. The
results doc carries the same stale pair (`a3e976af…` / 2,260,638 B).

More usefully than "update the number": that number is *inherently* ephemeral
and cannot be kept true. The zip contains the manifest, and the manifest embeds
`git.describe` (`e60603d-dirty`) and `worktree_clean: false`. Both change the
moment this work is committed, so the zip sha necessarily changes with it — any
absolute zip checksum written into that README is guaranteed to go stale on the
next commit, including a corrected one.

Recommendation (cheap, and for whoever picks it up rather than a rework cycle):
drop the parenthetical sha, or state that the value is commit-dependent and that
the checked property is the *equality of two runs*. The equivalent claim in
LOGBOOK 0530 (`fb36923d…`) is fine as-is — a logbook entry is a dated record of
what was measured then, not a live claim.

Not blocking: it is one illustrative number in a repo doc, the property it
illustrates is verified, and no number in the **shipped** artifact is wrong —
manifest and artifact README are exact to the byte.

## Note on the docs' framing (minor, no action needed)

`.scripts/packaging/README.md` and `build_slices`'s docstring present the
polluted-target build as producing `bab48d50…`, as though reuse yields a second
stable value. My X3 shows it yields an unstable one. The docs' conclusion is
unaffected and the fix is right; if that section is ever touched, "a reused
target directory makes the bytes unpredictable" is both truer and a stronger
argument for the dedicated target dir.

## Verdict

→ `done`. All AC met and independently verified: artifacts are reproducible
(measured across paths, target-dir paths, `CARGO_HOME`s and two full runs),
version-identifiable (contract read from the running binary, commit + toolchain +
checksums recorded), consumable (a real SwiftPM package resolves and runs
`probe_transfer` on macOS arm64), and measured for size. Pipeline is a make
target reusable by CI; all gates green.

The architecture, pipeline shape, Swift consumer proof, per-platform matrix and
the LTO crate-type override all stand. The one open item is a stale illustrative
checksum in a doc, recorded above as a follow-up.
