# Artifact packaging

How the shared Rust core reaches native consumers. One versioned source (the
workspace) produces every artifact; this directory owns what actually ships —
the shipped-target list, the crate-type the release binary is built as, the
debug-info policy, and the version metadata and checksums that make an artifact
attributable to a commit (NFR-052).

Owned by TASK-260715-3akqs8 (STORY-260715-2p879f, EPIC-260715-1poogc).

```sh
make package               # build + verify the artifacts
make package-reproducible  # build the shipped library at two paths, compare bytes
```

Output lands in `.temp/packaging/` (gitignored). Artifacts are built, never
committed: a checked-in binary is a binary nobody can attribute to a commit.

## What it produces

```
.temp/packaging/
  GramDriveCore/                  a self-contained SwiftPM package — the artifact
    Package.swift                 consumers depend on this, not on cargo
    GramDriveCore.xcframework/    macos-arm64 slice: staticlib + headers + modulemap
    Sources/GramDriveCore/        generated Swift bindings
    gramdrive-core-manifest.json  contract version, commit, toolchain, sizes
    README.md                     integration metadata for whoever receives it
  consumer/                       the minimal Swift package that proves the artifact
  target/                         packaging's own cargo target dir, wiped per build
  manifest.json                   the manifest plus sizes and checksums
  CHECKSUMS.sha256                sha256 of every shipped file (`shasum -c` format)
  GramDriveCore-<version>.zip     deterministic; its sha256 is the SwiftPM checksum
```

Measured for 0.1.0 / macos-arm64: XCFramework 7.95 MB, artifact 8.01 MB, zip
2.26 MB.

## Per-platform consumption

| Platform | Artifact | Status |
|---|---|---|
| macOS 14+ arm64 | `GramDriveCore.xcframework` + generated Swift, as a SwiftPM package | **built and verified here** |
| Windows | the `gramdrive-ffi` crate, as a direct Rust dependency | documented, no artifact by design |
| Linux | the `gramdrive-ffi` crate, as a direct Rust dependency | documented, no artifact by design |
| Android | `.so` per ABI + generated Kotlin | **deferred** — see below |
| iOS | device + simulator slices in the same XCFramework | **deferred** — see below |

**Windows and Linux consume the crate, not an artifact.** Their hosts (CfAPI,
FUSE) are Rust programs that link `gramdrive-ffi` as a normal path dependency
and call it as Rust — no UniFFI, no bindings, no packaging step. That is why the
crate keeps its `lib` (rlib) crate-type alongside `staticlib`/`cdylib`. They get
the crate's source and cargo's own reproducibility; there is nothing for this
pipeline to build for them, and inventing an artifact would add a versioning
seam where none is needed.

**Android is deferred, not stubbed.** Building it means the NDK, a
`cargo-ndk`-equivalent cross-build per ABI (arm64-v8a at minimum), an AAR or a
jniLibs layout, and the JNA/coroutines runtime the Kotlin bindings need — none
of which can be verified from a macOS-only support matrix (POL-5/DEC-017), and
all of which the bindings smoke gate already exercises at the *contract* level
(`.scripts/smoke/`, Kotlin consumer, TASK-260715-265gqq). A stub here would be a
build path nothing runs and nothing checks, which rots between now and the
Android platform epic. The pipeline is structured so that adding it is adding
ABIs to `SLICES` and an AAR assembly step, not a rewrite.

**iOS is deferred on the same grounds**, and additionally gated: DEC-012 makes
the cold-hydration strategy a release gate for iOS, and DEC-006 keeps TDLib out
of the File Provider extension. Device and simulator slices go into the same
XCFramework — `xcodebuild -create-xcframework` already takes multiple
`-library` pairs, which is why `SLICES` is a list.

## The three properties this pipeline exists to guarantee

Each fails loudly rather than degrading quietly.

### Reproducible

The same commit must produce the same bytes anywhere, at any time. Neither holds
by default. Both differences were measured here, not assumed, and both are
fixed — but the second one is not what it first looked like, so the measurements
are worth stating in full.

**Where it was built.** Rust embeds absolute source paths in debug info, and
`[profile.release] debug = "line-tables-only"` means the shipped archive carries
them. Without remapping, the same commit at two paths produced `b6c393fe…` and
`275d96ab…`.

`remap_rustflags()` remaps both the workspace and `CARGO_HOME`. Both are needed:
dependency code is compiled from the registry and lands in the archive too, so
remapping only the workspace leaves the build machine's home directory embedded
and reintroduces the difference on a machine whose home differs. `std` is already
remapped upstream to `/rustc/<hash>`. The flags go through
`CARGO_ENCODED_RUSTFLAGS`, not `RUSTFLAGS`: the encoded form is `\x1f`-separated,
so a repo path containing a space cannot silently split the remap into two broken
flags and leave the build succeeding without it.

**What else was built there first.** Remapping alone is not enough, and the
reason is not obvious. The shipped library's bytes depend on *prior state in the
target directory*. Measured at one fixed path, source byte-identical, only the
target directory varying:

| Build of the same commit | sha256 of `libgramdrive_ffi.a` | size |
|---|---|---|
| reusing the repo's `target/`, `cargo clean -p` first | `bab48d50…` | 7,920,568 B |
| **fresh target directory** | `110b1b9a…` | 7,920,584 B |

`110b1b9a…` is what a clean build produces at *every* path tried — the repo
checkout, two staged copies at different path lengths, and `/private/tmp` — and
across two different `CARGO_HOME`s. The path is a measured non-variable; the
reused target directory is the variable. The repo's `target/` accumulates 547
dependency artifacts, an `incremental/` directory, and a stale rlib and dylib
from ordinary `cargo build` runs; the delta between the two archives is confined
to LLVM's `.llvm.*` local-symbol suffixes, which thin LTO assigns and
`--remap-path-prefix` does not reach.

So the shipped library is built in **packaging's own target directory**
(`.temp/packaging/target`), wiped before every build, and the repo's `target/` is
left to the debug loop that owns it. `cargo clean -p` is not an alternative: it
drops the named crate's artifacts and keeps every dependency, which is exactly
the state that moves the bytes. The bindings generator deliberately stays on the
repo's `target/` — it is a host tool, nothing of it ships, and keeping its
`--features bindgen` build out of the packaging target directory is what keeps
that directory single-purpose.

The manifest reports this rather than asserting it: `path_independent` is
computed from what the build actually did (`clean_target_dir` and the recorded
remapping), so a change that reuses a target directory flips the field instead of
lying to consumers. The field records only the prefixes paths were remapped
*to* — the manifest ships inside the zip, and recording the `<from>` side would
put the build machine's paths back into the artifact through the metadata.

**When it was built.** A wall-clock build stamp is the one field that makes two
builds of one commit differ: it lands in the manifest, the manifest lands in the
zip, and the published checksum changes every run while nothing about the
software does. (This pipeline had exactly that bug, caught by running it twice.)
The artifact is stamped with its *source* date — `SOURCE_DATE_EPOCH` if set, else
the commit date — which is also the field a reader actually wants. The zip is
written with fixed timestamps and sorted entries for the same reason: a stock zip
of a byte-identical tree differs per run.

Together those make the whole artifact reproducible, not just the library. Two
full `make package` runs from scratch produce an identical
`GramDriveCore-0.1.0.zip` (`a3e976af…`).

`make package-reproducible` stages the build inputs to **two different paths**,
builds each from a clean target directory, and compares the bytes. Varying the
path is the point: an earlier version of this check built twice at the same path,
which tests determinism — real, but not the property the manifest asserts — and
so could not observe the axis it existed to cover. Both builds go through the
same procedure the shipped artifact does, because a check whose procedure differs
from the shipped build's covers some other build that happens to live in the same
file. The tie is visible: the check reports `110b1b9a…`, and that is the digest
of the library inside the shipped XCFramework.

### Version-identifiable

The manifest's contract version is **read from the built binary by running it**,
never parsed out of Rust source. The Swift verifier calls `contractVersion()`
and prints it; the pipeline records what it printed. A manifest cannot claim a
contract the shipped binary does not implement. Same principle as UniFFI library
mode: the binary is the source of truth, not any IDL.

Alongside it: `git describe`, the commit, whether the worktree was clean, the
resolved `uniffi` version (from the lockfile — the requirement in `Cargo.toml`
is a range, and the generator/runtime pair is a toolchain contract), and the
rustc version.

### Consumable

`swift-consumer/` is a real SwiftPM package that resolves a real path dependency
on the staged artifact, builds, and runs. It is the acceptance test: if the
artifact is missing a header, a modulemap, a slice, or the `Info.plist` SwiftPM
wants, resolution fails here rather than in a native host months later. It calls
`probe_transfer` and checks progress callbacks, because a package that compiles
but traps on the first call is not consumable.

The contract's *own* guarantees — async, structured errors, cancellation, and
the Kotlin side — belong to the bindings smoke gate (`.scripts/smoke/`,
TASK-260715-265gqq). This proves the packaging.

## Two decisions this pipeline settles

**Crate-type: the release build overrides it to `staticlib`.** The crate declares
`crate-type = ["lib", "staticlib", "cdylib"]` for its several consumers, and
cargo omits `-C lto` entirely when one rustc invocation also emits an rlib — so
the default release build of this crate silently ships *without* the thin LTO
`[profile.release]` asks for, and says nothing about it. `cargo rustc
--crate-type staticlib` builds only the type that ships and restores it
(verified: `-C lto=thin` appears in the rustc invocation only with the
override). This settles the caveat recorded in `[profile.release]` in
`Cargo.toml` and needs no architecture change — the manifest keeps every
crate-type its consumers need, and packaging asks for the one it ships.

**Debug info: shipped, not stripped.** Measured on the 0.1.0 macos-arm64 slice,
`strip -S` takes the archive from 7,920,584 B to 2,695,152 B — debug info is
~5.2 MB, two thirds of it. It stays, because that number is not the cost: the
linker pulls only referenced objects out of a static archive and debug info
lands in the consuming app's dSYM rather than the executable users download; it
compresses to near-nothing for distribution (the whole artifact zips to ~2.3 MB,
under the stripped archive alone); and it is what lets the app's `dsymutil`
resolve a crash inside the core to a line instead of a column of addresses.
Hosts that want a smaller link can strip at their link step.

## Requirements

macOS with Xcode (`xcodebuild`, `swift`) and the pinned Rust toolchain. POL-5
makes the Apple host the only v1 target; on any other platform the script exits
2 with that reason rather than producing a partial artifact.

Self-tests: `.scripts/tests/test_build_core_artifacts.py`, run by the `repo`
gate suite. They fake every subprocess, so they cover what the real pipeline
cannot be asked to stage on demand — a build that reports success but writes no
library, a verifier that prints nothing parseable — and they run on a machine
without Xcode.
