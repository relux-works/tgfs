# TASK-260715-rxjkpi — Build reproducible TDLib artifacts

**Status:** ready for review
**Target:** macOS 14+ arm64 (POL-5 / DEC-017 — the whole v1 support matrix)
**TDLib:** 1.8.66 @ `022d60202e446ad1287b9fb68e687c8a0760788b`

## What was built

A reproducible, attributable, from-source build pipeline for the pinned TDLib
**tdjson** library GramDrive's local Telegram source (EPIC-260715-2ptb18,
DEC-004) links against — mirroring the conventions of the existing
`.scripts/packaging/` core-artifact pipeline.

| File | Role |
|---|---|
| `.scripts/tdlib/build_tdlib.py` | The pipeline: fetch → build → stage → smoke → manifest/checksums. Python 3.11+, stdlib only. Runner-abstracted for faked-subprocess tests. |
| `.scripts/tdlib/README.md` | Deps (exact), pin, layout, per-platform posture, the three guarantees, CI reuse. |
| `.scripts/tdlib/link-smoke/` | Standalone Cargo crate (own `[workspace]`, **not** in the gramdrive workspace) that links libtdjson and drives the C JSON interface. |
| `.scripts/tests/test_build_tdlib.py` | 20 faked-subprocess self-tests, run by the `repo` gate suite (no Xcode/cmake/network needed). |
| `Makefile` | `tdlib`, `tdlib-smoke`, `tdlib-verify` targets. |

Output (gitignored `.temp/tdlib/out/`):

```
lib/libtdjson.dylib             23,661,456 B   @rpath install name
include/td/telegram/td_json_client.h
include/td/telegram/td_log.h
include/td/telegram/tdjson_export.h            (cmake-generated export header)
LICENSE_1_0.txt                                Boost Software License 1.0 (POL-6)
manifest.json                                  pin, versions, toolchain, linkage, checksums
CHECKSUMS.sha256                               shasum -c format
```

## Acceptance criteria → evidence

- **Pinned revision** — `TDLIB_COMMIT` (one constant). Immutable commit hash, not a tag (TDLib's last tag `v1.8.0` is 2022; lib is 1.8.66). Fetch is a shallow fetch of exactly that commit and is skipped (offline) when already checked out → "cached so rebuilds are incremental".
- **Reproducible** — Release build, `-ffile-prefix-map`, `ZERO_AR_DATE=1`, `@rpath` install name; ld64's content-hashed LC_UUID + ad-hoc signature are deterministic given identical content. `make tdlib-verify` builds twice from a clean tree and compares the library sha256. Manifest's `path_independent` is *derived* from what the build did, not asserted. Guarantee is attributability (NFR-052); cross-machine identical bytes are best-effort, not claimed.
- **Versioned** — `manifest.json` records the pinned commit, the source-declared version, and the **runtime** version read out of the running library by the smoke bin (1.8.66).
- **Checksummed** — sha256 of every shipped file in `manifest.json` and `CHECKSUMS.sha256` (both exclude themselves).
- **Licensed** — BSL-1.0 recorded in the manifest and TDLib's `LICENSE_1_0.txt` staged with the artifact (POL-6 permissive allow-list; POL-6 names BSL-1.0 explicitly).
- **Consumable by the Rust wrapper** — the link-smoke binary links `-ltdjson` (via `build.rs` + rpath) and runs against the staged artifact.
- **Documented deps / exact commands** — `.scripts/tdlib/README.md`; `manifest.json` `toolchain`.
- **CI-reusable per barycenter** — `make tdlib` is shorthand for the script, never a second copy of the commands.

## Link smoke output (verbatim)

```
linked deprecated symbols: td_json_client_create @ 0x10460dfd0, td_json_client_destroy @ 0x10460e024
created client id 1 via td_create_client_id
TDLib version: 1.8.66
```

The smoke drives the modern interface end to end (`td_execute` → silence logs,
`td_create_client_id` → client, `td_send` → `getOption "version"`, `td_receive`
→ read it; `getOption "version"` is answered pre-auth so no api_id/network is
needed) and references the deprecated `td_json_client_*` symbols for link proof
only — TDLib forbids mixing the two client interfaces in one process, so they
are not invoked against the live modern scheduler.

## Gates

`make check` (suite `all`): **8/8 passed** — toolchain, format, lint, test,
architecture, supply-chain (incl. POL-6 license check), traceability, scripts
(the 20 new self-tests). The link-smoke crate's `[workspace]` isolation keeps
libtdjson out of `cargo build --workspace`, so the gate stays green without the
artifact, Xcode, or a network.

## Reproducibility verify (`make tdlib-verify`)

Two clean builds of the same commit produced a byte-identical library:

```
REPRODUCIBLE: two clean builds of 022d60202e44 agree
  libtdjson.dylib sha256 1735ce834e68fac6c8d546d4808b58c41ca6b291625bf043239d28f9e9976eac
```

That digest matches the `manifest.json`-recorded library sha256 and the staged
`out/lib/libtdjson.dylib`, and `shasum -c CHECKSUMS.sha256` passes for all five
shipped files — the artifact is internally consistent. `tdlib-verify` stages
each build into a scratch tree, so it never clobbers the canonical `out/`.

## Recorded linkage (`otool -L`, in manifest)

```
/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib
/opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib
/usr/lib/libz.1.dylib
/usr/lib/libc++.1.dylib
/usr/lib/libSystem.B.dylib
```

## Downstream (out of scope here, noted for the reviewer)

- The dylib links OpenSSL/zlib **dynamically** (Homebrew absolute install names).
  A self-contained / static-OpenSSL variant plus notarization/signing belong to
  the release-signing task (TASK-260715-3bhbkv), not this build task.
- The safe tdjson wrapper (TASK-260715-2ulon7) consumes this artifact via
  `GRAMDRIVE_TDLIB_ARTIFACT_DIR` + an rpath, the same way the smoke bin does.
- Windows/Linux hosts build tdjson with their own platform toolchain; Android/iOS
  are deferred on the same grounds `.scripts/packaging/` records.
