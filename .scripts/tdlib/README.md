# TDLib artifact build

How the pinned TDLib **tdjson** library reaches GramDrive's local Telegram
source. GramDrive's local `DriveSource` (EPIC-260715-2ptb18, DEC-004) talks to
Telegram through TDLib's C JSON interface; this directory owns *what tdjson
artifact we ship and how it is attributable to a source commit* — the pinned
revision, the dependency and compiler policy, the produced library + headers +
license, and the version metadata and checksums (NFR-052).

Owned by TASK-260715-rxjkpi (STORY-260715-3elo6l, EPIC-260715-2ptb18). Sibling
of `.scripts/packaging/` and built on the same principles.

```sh
make tdlib              # fetch, build, stage, and prove the artifact
make tdlib-smoke        # re-run only the Rust link smoke against a staged artifact
make tdlib-verify       # build twice from clean, compare the library bytes
```

Output lands in `.temp/tdlib/` (gitignored). The artifact is built, never
committed: a checked-in binary is a binary nobody can attribute to a commit, and
POL-6 needs the license provenance recorded where the bytes are produced.

## The pin

TDLib is pinned to an immutable commit hash in exactly one place —
`TDLIB_COMMIT` in `build_tdlib.py`:

```
repo:   https://github.com/tdlib/td.git
commit: 022d60202e446ad1287b9fb68e687c8a0760788b   (master, resolved 2026-07-17)
```

A commit, not a tag: TDLib tags rarely (the last git tag `v1.8.0` is from 2022,
while the library is on 1.8.x well past it), so the ecosystem pins commits.
Bumping TDLib is editing that one constant and re-running `make tdlib`.

## What it produces

```
.temp/tdlib/
  src/                          TDLib checkout, pinned to TDLIB_COMMIT (cached)
  build/                        cmake build tree (cached; rebuilds are incremental)
  out/                          the staged, self-describing artifact
    lib/libtdjson.dylib         the shared C JSON client library (@rpath install name)
    include/td/telegram/        td_json_client.h, td_log.h, tdjson_export.h
    LICENSE_1_0.txt             TDLib's Boost Software License 1.0 (POL-6)
    manifest.json               pin, version, toolchain, sizes, checksums, linkage
    CHECKSUMS.sha256            sha256 of every shipped file (`shasum -c` format)
```

`tdjson` is TDLib's **shared** library target: it links every TDLib sub-library
(tdcore, tdactor, tdnet, tddb, tdutils, …) into itself and exports only the C
JSON interface, so the single dylib is the whole client. The consumer compiles
against the three public headers and loads the one dylib.

## Dependencies (exact)

macOS arm64 host with:

| Tool | Provides | Install |
|---|---|---|
| Xcode / command line tools | `clang`, `install_name_tool`, `otool`, `xcrun`, the macOS SDK (**zlib** lives here) | Xcode from the App Store, or `xcode-select --install` |
| cmake | configure/build | `brew install cmake` |
| gperf | TDLib's build-time perfect-hash generator | `brew install gperf` |
| OpenSSL 3 | libcrypto/libssl TDLib links | `brew install openssl@3` |

`build_tdlib.py` resolves OpenSSL with `brew --prefix openssl@3` (Homebrew keeps
it keg-only, off the default search path) and passes it to cmake as
`OPENSSL_ROOT_DIR`. Override with `OPENSSL_ROOT_DIR=…` for a vendored or non-brew
OpenSSL. The exact cmake invocation is in `configure_and_build`; the resolved
tool versions are recorded in `manifest.json` under `toolchain`.

## Per-platform consumption

| Platform | Artifact | Status |
|---|---|---|
| macOS 14+ arm64 | `libtdjson.dylib` + C headers, staged here | **built and verified here** |
| Windows / Linux | the tdjson source built by the platform host's own toolchain | documented, no artifact by design |
| Android | `.so` per ABI via the NDK | **deferred** |
| iOS | device + simulator slices, TDLib in the containing app only (DEC-006) | **deferred** |

POL-5/DEC-017 make macOS 14+ arm64 the entire v1 support matrix; on any other
host the script exits 2 with that reason rather than producing a partial
artifact. Windows/Linux native hosts (CfAPI, FUSE) are Rust programs that build
tdjson with their own platform toolchain; there is nothing for this macOS
pipeline to produce for them, and inventing a cross-build here would be a path
nothing runs and nothing checks — the same reasoning `.scripts/packaging/`
records for Android/iOS. Adding a platform is adding a target here, not a
rewrite.

The dedicated x86_64 CI/signing runner consumes an artifact built on that
arm64 host through `restore_pinned_artifact.py`. The runner-local cache key is
derived from `build_tdlib.py`; a miss fails actionably instead of rebuilding or
relabeling. Restoration validates the exact file/checksum/manifest inventory,
the pinned TDLib source and target, and arm64-only `file(1)` output before an
atomic workspace replacement. CI then cross-links the shipped target with
`make tdlib-smoke-link`; it never attempts to execute arm64 code on Intel.

## The three properties this pipeline guarantees

Each fails loudly rather than degrading quietly, matching `.scripts/packaging/`.

### Attributable (NFR-052)

The guarantee is that the bytes tie back to their inputs. `manifest.json`
records the pinned TDLib commit, the TDLib version (both the source-declared
number and the **runtime** version the smoke test reads out of the built
library), the full toolchain (cmake, clang, OpenSSL, zlib, gperf), the target
and deployment floor, the license, the dylib's dynamic dependencies (`otool
-L`), and a sha256 for every shipped file. `CHECKSUMS.sha256` is the same in
`shasum -c` form. The manifest and checksum file exclude themselves — a file
cannot list its own hash.

### Reproducible (best-effort, honestly scoped)

Byte-identical rebuild is pursued **same-machine** and is what CI caching
depends on:

- **Release** build type keeps debug info — and the source paths it would embed
  — out of the objects, and sets `-DNDEBUG` so path-carrying assertions drop.
- `-ffile-prefix-map=<src>=/tdlib` remaps what source paths remain (the C++
  analogue of the core pipeline's rustflag remap). The manifest records only the
  prefix paths were remapped *to*; recording the `<from>` side would put this
  machine's path back into the metadata.
- `ZERO_AR_DATE=1` zeroes ar/libtool archive timestamps.
- ld64 derives the dylib's `LC_UUID` from a content hash and applies an ad-hoc
  code signature, both deterministic given identical content.

`make tdlib-verify` builds twice from a **clean build tree** (the axis most
likely to move the bytes) sharing the one immutable source checkout, and
compares the library's sha256. `manifest.json`'s `path_independent` is *derived*
from what the build did (`clean_build_tree` and the recorded remap), so a build
that reuses the tree reports the weaker claim instead of asserting the stronger
one. Cross-machine identical bytes depend on identical clang and OpenSSL and are
**not** claimed — attributability is.

### Consumable

`link-smoke/` is a real Rust binary that links `libtdjson`, drives the C JSON
interface, and reads the version out of the running library. It is the
acceptance test: if the artifact is missing the dylib, a header, or a symbol, it
fails to link or load *here* rather than in the real tdjson wrapper
(`gramdrive-source-tdjson`, STORY-260715-3elo6l) later. See
`link-smoke/src/main.rs` for exactly which symbols it exercises and why it only
*references* (does not call) the deprecated single-client API.

## The link-smoke crate is not in the workspace

`link-smoke/` carries its own empty `[workspace]` table so cargo treats it as a
standalone root and never attaches it to the gramdrive workspace at `../../..`.
Linking libtdjson must not leak into `cargo build --workspace` / `make check`,
which run on machines that never built this artifact — the same isolation
`gramdrive-source-tdjson` keeps with its env-gated `build.rs` (linkage only
when `GRAMDRIVE_TDLIB_ARTIFACT_DIR` is set; `make tdjson-smoke`). `make
check` therefore stays
green without the artifact, the network, or Xcode; the only thing the gate runs
is the faked-subprocess self-test below.

## CI reuse (barycenter pattern)

`make tdlib` is shorthand for `python3 .scripts/tdlib/build_tdlib.py`, never a
second copy of the commands — the same one-entrypoint rule the acceptance runner
and `make package` follow, so "it builds in CI" and "it builds on my machine"
are the same sentence. Building the real artifact needs a macOS runner with the
tools above; it is its own CI job, not a step of `make check` (which needs
neither Xcode nor the network).

## Requirements and self-tests

`build_tdlib.py`: Python 3.11+, stdlib only. Self-tests live at
`.scripts/tests/test_build_tdlib.py` and run in the `repo` gate suite (`make
check-repo`). They fake every subprocess, so they cover what the real pipeline
cannot be asked to stage on demand — a build that reports success but writes no
dylib, a smoke run that prints nothing parseable, a fetch that must be skipped
because the pin is already checked out — and they run on a machine without
Xcode, cmake, or a network.
