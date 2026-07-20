#!/usr/bin/env python3
"""Package the GramDrive shared core for native consumers.

One versioned source (the Rust workspace) produces every artifact a native host
consumes. This script owns what actually ships: the shipped-target list, the
crate-type the release binary is built as, the debug-info and stripping policy,
and the version metadata and checksums that make an artifact attributable to a
commit (NFR-052). Owned by TASK-260715-3akqs8.

    python3 .scripts/packaging/build_core_artifacts.py
    python3 .scripts/packaging/build_core_artifacts.py --skip-verify
    python3 .scripts/packaging/build_core_artifacts.py --check-reproducible
    python3 .scripts/packaging/build_core_artifacts.py --host-test-slice

What it produces, under .temp/packaging/ (gitignored):

    GramDriveCore/                  a self-contained SwiftPM package
      Package.swift                 consumers depend on this, not on cargo
      GramDriveCore.xcframework/    macos-arm64 slice: staticlib + headers
      Sources/GramDriveCore/        generated Swift bindings
      gramdrive-core-manifest.json  contract version, commit, sizes, checksums
      README.md                     integration metadata
    consumer/                       the minimal Swift package that proves it
    target/                         packaging's own cargo target dir (see build_slices)
    manifest.json                   same manifest, outside the artifact
    CHECKSUMS.sha256                sha256 of every shipped file
    GramDriveCore-<version>.zip     deterministic; sha256 is the SPM checksum

Three properties this pipeline exists to guarantee, each of which fails loudly
rather than silently degrading:

  * **Reproducible.** The same commit produces the same bytes at any path and
    any time. Three things are required, each measured rather than assumed:
    rustflags remap the checkout path and CARGO_HOME out of the debug info, the
    shipped library is built in a dedicated target directory that is wiped first
    (a reused one changes the bytes -- see build_slices), and the artifact is
    stamped with its source date rather than the build time, with the zip
    written at fixed timestamps. `--check-reproducible` builds at two different
    paths from clean and compares.
  * **Version-identifiable.** The manifest's contract version is read from the
    built artifact by running it, never parsed out of Rust source, so the
    manifest cannot describe a contract the binary does not implement.
  * **Consumable.** The Swift verifier is a real SwiftPM package resolving a
    real dependency on the staged artifact. It is the acceptance test.

Host-architecture reality (TASK-260719-1dwaj8). The shipped slice list is arm64
only, but CI runs on an x86_64 self-hosted mac. Two modes cover it, both
recorded in the manifest rather than silently degrading:

  * default staging on a non-arm64 host cross-compiles the shipped arm64 slice
    and downgrades the verifier to `cross-link-only`: the consumer package is
    cross-built against the artifact (a real resolve + link proof) but cannot
    execute, so the contract version stays `unverified`. The runtime probe for
    the same commit lives in native-ci's apple-build-test leg, which uses:
  * `--host-test-slice`, which additionally builds a host-arch twin of the same
    source and lipo's it into the staged slice, so the verifier and the apple
    test suite can execute natively. That staging is for CI testing only and
    says so in its manifest and README; release staging never passes the flag.

Requires: cargo, git, xcodebuild and swift on PATH (macOS; POL-5 makes the
Apple host the only v1 target). Python 3.11+, stdlib only.

Exit codes: 0 packaged, 1 a step failed, 2 the run could not start.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import zipfile
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_CANNOT_START = 2

OUT_ROOT = Path(".temp") / "packaging"

# The crate that is packaged, and the names its build and its bindings use.
FFI_CRATE = "gramdrive-ffi"
LIB_STEM = "libgramdrive_ffi"
# uniffi.toml pins these; they are the module names the generated sources and
# modulemap declare, so packaging cannot pick its own.
SWIFT_MODULE = "GramDriveCore"
FFI_MODULE = "GramDriveCoreFFI"
XCFRAMEWORK_NAME = f"{SWIFT_MODULE}.xcframework"
MANIFEST_NAME = "gramdrive-core-manifest.json"
# The staged artifact's directory name is not cosmetic: SwiftPM takes a path
# dependency's package identity from its directory name, so a consumer writing
# `.package(path: "../GramDriveCore")` must find exactly this.
ARTIFACT_DIR_NAME = SWIFT_MODULE


@dataclass(frozen=True)
class Slice:
    """One architecture slice of the shipped artifact."""

    #: Rust target triple, passed to `cargo rustc --target`.
    triple: str
    #: Directory name inside the XCFramework. Cosmetic to xcodebuild (it writes
    #: its own identifier into Info.plist), recorded here so the manifest and
    #: the layout agree on what to call the slice.
    label: str


# The shipped-target list, deliberately in exactly one place.
#
# POL-5/DEC-017 make macOS 14+ arm64 the entire v1 support matrix: Intel is
# explicitly out of scope, and iOS device/simulator slices are defined when iOS
# enters scope (DEC-012 gates that on the cold-hydration decision). rust-toolchain.toml
# deliberately carries no `targets` entry and defers to this list; adding a
# platform here means adding it there too.
SLICES: tuple[Slice, ...] = (Slice(triple="aarch64-apple-darwin", label="macos-arm64"),)

# Rust target triples by macOS host architecture (`platform.machine()` spelling).
# Used only to decide whether this host can execute the shipped slice (verifier
# mode) and to name the --host-test-slice twin; the shipped list is SLICES and
# nothing else (TASK-260719-1dwaj8).
HOST_TRIPLES: dict[str, str] = {
    "arm64": "aarch64-apple-darwin",
    "x86_64": "x86_64-apple-darwin",
}


def swift_arch(triple: str) -> str:
    """The `swift build --arch` spelling of a Rust triple's architecture."""
    prefix = triple.split("-", 1)[0]
    return {"aarch64": "arm64"}.get(prefix, prefix)

# The macOS deployment target the artifact claims (POL-5). Passed to the Rust
# build so the objects in the staticlib carry the same floor the Swift package
# declares; a mismatch surfaces as linker warnings in a native host.
MACOSX_DEPLOYMENT_TARGET = "14.0"

# Cargo's separator for CARGO_ENCODED_RUSTFLAGS: an ASCII unit separator, which
# cannot occur in a path or a flag, which is the whole point of the encoded form.
RUSTFLAG_SEPARATOR = "\x1f"
ENCODED_RUSTFLAGS = "CARGO_ENCODED_RUSTFLAGS"

# Packaging's own target directory, under the (gitignored) output root. The
# shipped library is built here and nowhere else; see build_slices.
TARGET_DIR_NAME = "target"

# Exactly the inputs the shipped build reads, copied by stage_build_inputs when
# --check-reproducible builds the same source at another path. A file the build
# needs and this list omits makes that build fail loudly rather than silently
# check something other than what ships; anything else in the repo (docs, specs,
# .git, the board) is not a build input and staging it would only slow the copy.
BUILD_INPUTS: tuple[str, ...] = ("crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml")

# Never copied into a staged tree: build output and local editor droppings. The
# packaging verifier package lives in .scripts/ (source), but a `swift build`
# run against it by hand leaves .build/ and Package.resolved behind, and copying
# those into the tree the pipeline then builds would carry stale SwiftPM state
# into the artifact's own acceptance test.
COPY_EXCLUDES: tuple[str, ...] = (".build", "Package.resolved", ".DS_Store", "target", "__pycache__")

Runner = Callable[[Sequence[str], Path, dict[str, str] | None], tuple[int, str]]


def default_runner(
    argv: Sequence[str], cwd: Path, env: dict[str, str] | None = None
) -> tuple[int, str]:
    """Run argv in cwd, returning (exit code, combined output)."""
    try:
        proc = subprocess.run(
            list(argv),
            cwd=cwd,
            capture_output=True,
            text=True,
            env=env,
        )
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found on PATH\n"
    return proc.returncode, proc.stdout + proc.stderr


class StepFailed(Exception):
    """A packaging step failed; carries what to print and nothing else."""


@dataclass(frozen=True)
class BuildRecord:
    """What the shipped build did, as opposed to what it was meant to do.

    The manifest's reproducibility claim is derived from this rather than
    written as a literal, so the file that ships to consumers cannot assert a
    property of a build that did not happen.
    """

    #: Whether the target directory was wiped before this build.
    clean_target_dir: bool
    #: The prefixes build paths were rewritten to (never the local paths they
    #: were rewritten *from*; see remapped_prefixes).
    remapped_to: tuple[str, ...]


def caller_rustflags(env: dict[str, str]) -> list[str]:
    """Whatever rustflags the caller set, as a list, in cargo's own precedence.

    Cargo reads CARGO_ENCODED_RUSTFLAGS in preference to RUSTFLAGS and ignores
    the latter entirely when both are set, so honoring them in the other order
    would silently drop a caller's flags.
    """
    encoded = env.get("CARGO_ENCODED_RUSTFLAGS")
    if encoded is not None:
        return [flag for flag in encoded.split(RUSTFLAG_SEPARATOR) if flag]
    return env.get("RUSTFLAGS", "").split()


def remap_rustflags(
    repo_root: Path, cargo_home: Path, base: Sequence[str] = ()
) -> list[str]:
    """Rustflags that keep the checkout path out of the shipped bytes.

    Rust embeds absolute source paths in debug info, and `[profile.release]
    debug = "line-tables-only"` means the shipped staticlib carries them.
    Measured: without remapping, the same commit built at two paths produced two
    different libraries (b6c393fe vs 275d96ab); with both prefixes remapped, two
    paths produce the same bytes. Two prefixes are needed because dependency
    code is compiled from CARGO_HOME and lands in the archive too; remapping
    only the workspace leaves the registry path embedded and reintroduces the
    difference on a machine whose home directory differs. std is already
    remapped to /rustc/<hash> upstream.

    This is necessary but not sufficient on its own: remapping only rewrites
    debug info, and a *reused* target directory moves the bytes for a different
    reason entirely (build_slices). Both are required for the manifest's
    path_independent claim, which is why that field is computed from both.

    `base` preserves the caller's flags and comes first, so an explicit caller
    flag cannot be silently overridden by ours -- rustc takes the last
    occurrence of a repeated flag, and these are additive anyway.
    """
    return [
        *base,
        f"--remap-path-prefix={repo_root}=/gramdrive",
        f"--remap-path-prefix={cargo_home}=/cargo",
    ]


def remapped_prefixes(env: dict[str, str]) -> tuple[str, ...]:
    """The prefixes an already-built environment rewrote build paths *to*.

    Read back out of the environment the build ran with, so the manifest reports
    the remapping that happened rather than the one this file intended.

    Only the destinations, never the `<from>` side. The source side is this
    machine's checkout and home directory -- exactly what remapping exists to
    keep out of the shipped bytes -- and the manifest ships inside the zip, so
    recording the pairs would reintroduce through the metadata the very paths
    the build stripped from the binary. The destinations are also the more
    useful half: someone reproducing this artifact needs to know to map their
    own checkout to /gramdrive and their CARGO_HOME to /cargo.
    """
    flags = env.get(ENCODED_RUSTFLAGS, "").split(RUSTFLAG_SEPARATOR)
    marker = "--remap-path-prefix="
    return tuple(
        flag.removeprefix(marker).rsplit("=", 1)[1]
        for flag in flags
        if flag.startswith(marker) and "=" in flag.removeprefix(marker)
    )


def build_env(
    repo_root: Path, target_dir: Path, environ: dict[str, str] | None = None
) -> dict[str, str]:
    """The environment the shipped build runs under.

    CARGO_ENCODED_RUSTFLAGS rather than RUSTFLAGS: the encoded form is
    \\x1f-separated, so a flag or a repo path containing a space survives it.
    The space-joined form would split into two broken flags and lose the remap
    silently -- the worst failure mode available, since the build still succeeds
    and only the reproducibility quietly stops holding. RUSTFLAGS is dropped
    from the environment rather than left to sit unread next to the encoded form
    it no longer affects.

    CARGO_TARGET_DIR points the build at packaging's own target directory; see
    build_slices for why the shipped library may not share the repo's.
    """
    env = dict(environ if environ is not None else os.environ)
    cargo_home = Path(env.get("CARGO_HOME") or (Path(env.get("HOME", "~")) / ".cargo"))
    flags = remap_rustflags(repo_root, cargo_home, caller_rustflags(env))
    env[ENCODED_RUSTFLAGS] = RUSTFLAG_SEPARATOR.join(flags)
    env.pop("RUSTFLAGS", None)
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["MACOSX_DEPLOYMENT_TARGET"] = MACOSX_DEPLOYMENT_TARGET
    # Deterministic diagnostics ordering; also keeps the recorded logs stable.
    env["LC_ALL"] = "C"
    return env


def cargo_staticlib_argv(triple: str) -> tuple[str, ...]:
    """The release build command for one slice.

    `--crate-type staticlib` is load-bearing and not a style choice. The crate
    declares `crate-type = ["lib", "staticlib", "cdylib"]` for its several
    consumers, and cargo omits `-C lto` entirely when one rustc invocation also
    produces an rlib -- so the default release build of this crate silently
    ships without the LTO the profile asks for. Overriding the crate-type to the
    single type that is actually shipped restores it (verified: `-C lto=thin`
    appears in the rustc invocation only with this override). This is the
    settlement of the caveat recorded in [profile.release] in Cargo.toml, and it
    needs no architecture change: the manifest keeps every crate-type its
    consumers need, and packaging asks for the one it ships.
    """
    return (
        "cargo",
        "rustc",
        "-p",
        FFI_CRATE,
        "--release",
        "--target",
        triple,
        "--crate-type",
        "staticlib",
    )


def bindgen_argv(library: Path, out_dir: Path) -> tuple[str, ...]:
    """Generate Swift bindings from the library that is actually shipped.

    Library mode against the release staticlib, not the debug dylib: UniFFI
    embeds per-API checksums that make a bindings/library mismatch fail at load
    time, so the only safe source for the shipped bindings is the shipped
    binary. --no-format because ktlint/swiftformat are not build requirements
    and their absence must not fail packaging (the generator only warns).
    """
    return (
        "cargo",
        "run",
        "--quiet",
        "-p",
        FFI_CRATE,
        "--features",
        "bindgen",
        "--bin",
        "uniffi-bindgen",
        "--",
        "generate",
        "--library",
        str(library),
        "--language",
        "swift",
        "--out-dir",
        str(out_dir),
        "--no-format",
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def checksum_tree(root: Path) -> dict[str, str]:
    """sha256 of every file under root, keyed by POSIX path relative to it.

    Sorted so the output is stable across filesystems: a checksum file whose
    line order depends on readdir order is a diff that lies.
    """
    return {
        str(path.relative_to(root).as_posix()): sha256_file(path)
        for path in sorted(root.rglob("*"))
        if path.is_file() and not path.is_symlink()
    }


def format_checksums(checksums: dict[str, str]) -> str:
    """Render checksums in `shasum -a 256 -c` format."""
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items()))


def tree_size(root: Path) -> int:
    return sum(p.stat().st_size for p in root.rglob("*") if p.is_file() and not p.is_symlink())


def write_deterministic_zip(source_dir: Path, zip_path: Path, prefix: str) -> None:
    """Zip source_dir reproducibly.

    A stock zip embeds mtimes and readdir order, which would make the archive of
    a byte-identical artifact differ per run and defeat the point of checksums.
    Entries are sorted and stamped with a fixed timestamp; the sha256 of the
    result is stable, and is exactly the value SwiftPM's
    `binaryTarget(url:checksum:)` expects.
    """
    # 1980-01-01, the zero of the DOS timestamp zip stores. Any fixed value
    # works; this one is the conventional choice and cannot be mistaken for a
    # real build time.
    fixed_time = (1980, 1, 1, 0, 0, 0)
    files = sorted(p for p in source_dir.rglob("*") if p.is_file() and not p.is_symlink())
    with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in files:
            arcname = f"{prefix}/{path.relative_to(source_dir).as_posix()}"
            info = zipfile.ZipInfo(arcname, date_time=fixed_time)
            info.compress_type = zipfile.ZIP_DEFLATED
            # Preserve the executable bit, drop everything else about the file's
            # local identity (owner, atime) that would vary between machines.
            mode = 0o755 if os.access(path, os.X_OK) else 0o644
            info.external_attr = mode << 16
            archive.writestr(info, path.read_bytes())


def artifact_package_swift(*, tdjson: bool) -> str:
    """The staged artifact's Package.swift.

    With `tdjson` (the env-gated real-linkage staging), the Swift target
    declares `-ltdjson`: the staticlib references the tdjson symbols, and a
    dependency package may not carry unsafe linker flags, so the library
    *name* is declared here and the search path is the consumer build's
    `LIBRARY_PATH` (which ld64 honors). The dylib itself is staged in the
    artifact's `lib/` with an absolute install name, so locally-run
    consumers (the verifier, the smokes) load it without rpath surgery; the
    app bundle rewrites the reference to @rpath when it embeds the library.
    """
    linker = "\n            linkerSettings: [.linkedLibrary(\"tdjson\")]," if tdjson else ""
    target = (
        f'.target(\n            name: "{SWIFT_MODULE}",\n'
        f'            dependencies: ["{FFI_MODULE}"],{linker}\n        )'
        if tdjson
        else f'.target(name: "{SWIFT_MODULE}", dependencies: ["{FFI_MODULE}"])'
    )
    return f"""// swift-tools-version:5.9
//
// GramDrive shared core -- generated by .scripts/packaging/build_core_artifacts.py.
// Do not edit: this file is rebuilt from the Rust workspace on every package run.
//
// Native hosts depend on this package and never on the Rust workspace, cargo,
// or the FFI crate's layout. The two targets mirror how UniFFI splits a
// binding: a C module carrying the compiled core, and the generated Swift that
// wraps it. Only `{SWIFT_MODULE}` is a product; `{FFI_MODULE}` is an
// implementation detail that consumers must not import.
//
// Version, commit and checksums: {MANIFEST_NAME}. Integration notes: README.md.

import PackageDescription

let package = Package(
    name: "{SWIFT_MODULE}",
    // POL-5/DEC-017: macOS 14+ arm64 is the v1 support matrix.
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "{SWIFT_MODULE}", targets: ["{SWIFT_MODULE}"])
    ],
    targets: [
        .binaryTarget(name: "{FFI_MODULE}", path: "{XCFRAMEWORK_NAME}"),
        {target}
    ]
)
"""


def render_artifact_readme(manifest: dict) -> str:
    """The integration metadata that ships inside the artifact."""
    slices = "\n".join(
        f"| `{entry['label']}` | `{entry['triple']}` | {entry['staticlib_bytes']:,} B |"
        for entry in manifest["slices"]
    )
    version = manifest["contract_version"]
    host_test_note = ""
    if manifest.get("host_test_slice"):
        twin = manifest["host_test_slice"]["triple"]
        host_test_note = (
            "\n> **CI test staging -- not the shipped shape.** This staging "
            f"carries an additional `{twin}` twin lipo'd into the archive so a "
            "CI host that cannot execute the shipped slice can still run the "
            "verifier and the native test suite against this source. Release "
            "staging never includes it (TASK-260719-1dwaj8).\n"
        )
    return f"""# GramDriveCore
{host_test_note}

The GramDrive shared Rust core, packaged for native consumers. Generated by
`.scripts/packaging/build_core_artifacts.py`; do not edit by hand.

## Identity

| | |
|---|---|
| Contract version | `{version}` |
| Verify mode | `{manifest.get("verify_mode", "native-run")}` |
| Crate version | `{manifest["crate_version"]}` |
| Commit | `{manifest["git"]["describe"]}` |
| Built from clean worktree | `{manifest["git"]["worktree_clean"]}` |
| UniFFI | `{manifest["toolchain"]["uniffi"]}` |
| Rust | `{manifest["toolchain"]["rustc"]}` |

The contract version is read from the built binary by calling
`contractVersion()`, not from source. Native hosts assert the major at startup:
UniFFI's embedded API checksums already make a bindings/library mismatch fail at
load time, and the contract major is what catches an intentional break.

## Slices

| Slice | Target triple | Static library |
|---|---|---|
{slices}

macOS 14+ arm64 only (POL-5/DEC-017). Intel is out of scope for v1; iOS
device/simulator slices are added when iOS enters scope.

## Consuming it

SwiftPM, as a path or checked-in dependency:

```swift
dependencies: [.package(path: "path/to/{SWIFT_MODULE}")]
```

or as a remote binary artifact, where the checksum is the zip's sha256 in
`CHECKSUMS.sha256` (the same value `swift package compute-checksum` prints):

```swift
.binaryTarget(name: "{FFI_MODULE}", url: "...", checksum: "...")
```

Import only the `{SWIFT_MODULE}` product. `{FFI_MODULE}` is the raw C surface
and is not a supported import.

Windows and Linux hosts do not consume this artifact: they depend on the
`{FFI_CRATE}` crate directly as a Rust dependency, which is why the crate keeps
its `lib` crate-type. Android consumes a `.so` plus the Kotlin bindings, which
this pipeline does not build yet -- both are covered in the repo README.

## Debug info

The static library carries line-tables-only debug info (`[profile.release]` in
the workspace manifest) and is deliberately **not** stripped.

Measured on the 0.1.0 macos-arm64 slice: 7,920,584 B as shipped against
2,695,152 B after `strip -S`, so debug info is ~5.2 MB, two thirds of the
archive. It stays anyway, because that number is not what it costs:

* **It costs nothing in the consuming app's binary.** The linker pulls only
  referenced objects out of a static archive, and debug info lands in the app's
  dSYM rather than the executable users download.
* **It costs almost nothing to distribute.** Debug info compresses well: the
  whole artifact zips to ~2.3 MB, under the size of the stripped archive alone.
* **It buys symbolication.** A static archive's debug info is what lets the
  consuming app's `dsymutil` resolve a crash inside the core to a line. Strip it
  and a crash report from the Rust core is a column of addresses.

Hosts that want a smaller link can strip at their own link step; that choice
belongs to whoever ships the app, not to this artifact.
"""


def parse_verifier_report(stdout: str) -> dict:
    """Pull the verifier's JSON line out of its stdout.

    SwiftPM and the runtime may print around it, so the report is located rather
    than assumed to be the whole of stdout: the last line that parses as a JSON
    object with the key we require.
    """
    for line in reversed(stdout.strip().splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            report = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(report, dict) and "contract_version" in report:
            return report
    raise StepFailed(
        "the Swift verifier printed no JSON report; it must print a single JSON "
        f"object containing 'contract_version'. stdout was:\n{stdout}"
    )


def reproducibility_record(record: BuildRecord | None) -> dict:
    """What this run can honestly say about reproducing its own bytes.

    `path_independent` is computed from the build that actually ran, not
    asserted. Both conditions are necessary and were measured to be jointly
    sufficient (2026-07-17, LOGBOOK 0552):

      * remapped prefixes keep the checkout path out of the debug info;
      * a wiped, dedicated target directory keeps prior build state out of the
        LTO'd bytes.

    Drop either and the claim is false, so a future change that reuses a target
    directory or stops remapping flips this field rather than lying in a file
    that ships to consumers. The claim is about *this* build's procedure; the
    check that varies the path axis and proves it is --check-reproducible.

    A missing record means no build happened, which is not a reproducible build
    -- the honest answer there is false, not an omitted field a reader would
    have to notice.
    """
    clean_target_dir = record is not None and record.clean_target_dir
    remapped_to = list(record.remapped_to) if record is not None else []
    path_independent = clean_target_dir and bool(remapped_to)
    return {
        "path_independent": path_independent,
        "clean_target_dir": clean_target_dir,
        "path_prefixes_remapped_to": remapped_to,
        "verified_by": (
            "build_core_artifacts.py --check-reproducible, which stages this source "
            "to two different paths, builds each from a clean target directory, and "
            "compares the bytes"
        ),
        "note": (
            "Byte-identity holds across build paths and across CARGO_HOMEs for a given "
            "toolchain (see the toolchain field), and across time because the artifact "
            "is stamped with its source date rather than the build time. It requires a "
            "clean target directory: a reused one changes LLVM's .llvm.* local-symbol "
            "suffixes, which --remap-path-prefix does not reach."
        ),
    }


def build_manifest(
    *,
    contract_version: str,
    crate_version: str,
    git: dict,
    toolchain: dict,
    slices: list[dict],
    source_date: str,
    reproducible: dict,
    verify_mode: str = "native-run",
    host_test_slice: dict | None = None,
) -> dict:
    """The artifact's identity record.

    `contract_version` is the value the built binary reported when it ran, not a
    value read from source; see parse_verifier_report and the module docstring.

    The date recorded is the *source* date, not the wall clock: see
    source_date(). A build timestamp here would be the one field that makes two
    builds of one commit differ, which is exactly the property this artifact
    claims not to have.

    `reproducible` is passed in rather than built here because it describes what
    the build actually did; see reproducibility_record.

    `verify_mode` and `host_test_slice` record how this staging was proven
    (TASK-260719-1dwaj8): a cross-linking CI host cannot run the verifier, and a
    host-test staging carries a slice that never ships. Both are facts about
    *this* build a reader could not otherwise recover from the artifact.
    """
    return {
        "schema": 1,
        "name": SWIFT_MODULE,
        "contract_version": contract_version,
        "crate_version": crate_version,
        "git": git,
        "toolchain": toolchain,
        "slices": slices,
        "verify_mode": verify_mode,
        "host_test_slice": host_test_slice,
        "source_date": source_date,
        "reproducible": reproducible,
    }


class Packager:
    """Runs the pipeline. Every subprocess goes through `self.runner`."""

    def __init__(
        self,
        repo_root: Path,
        out_dir: Path,
        *,
        runner: Runner = default_runner,
        echo: Callable[[str], None] = print,
        environ: dict[str, str] | None = None,
    ):
        self.repo_root = repo_root
        self.out_dir = out_dir
        self.runner = runner
        self.echo = echo
        self.environ = environ
        self.log_dir = out_dir / "logs"
        #: What build_slices actually did, for the manifest to report rather
        #: than restate. None until it has run: the manifest describes a build,
        #: so there is nothing honest to say about one that has not happened.
        self.build_record: BuildRecord | None = None

    def run(self, name: str, argv: Sequence[str], *, cwd: Path | None = None, env=None) -> str:
        self.echo(f"--- {name}: {' '.join(str(a) for a in argv)}")
        code, output = self.runner(argv, cwd or self.repo_root, env)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        (self.log_dir / f"{name}.log").write_text(output, encoding="utf-8")
        if code != 0:
            raise StepFailed(
                f"{name} failed (exit {code}); log: {self.log_dir / f'{name}.log'}\n"
                f"{output[-4000:]}"
            )
        return output

    # -- inputs ----------------------------------------------------------

    def git_info(self) -> dict:
        code, describe = self.runner(
            ("git", "describe", "--tags", "--always", "--dirty"), self.repo_root, None
        )
        _, commit = self.runner(("git", "rev-parse", "HEAD"), self.repo_root, None)
        status_code, status = self.runner(("git", "status", "--porcelain"), self.repo_root, None)
        return {
            "describe": describe.strip() if code == 0 else "unknown",
            "commit": commit.strip() or None,
            "worktree_clean": (status.strip() == "") if status_code == 0 else None,
        }

    def source_date(self) -> str:
        """The date the artifact is stamped with: the source's, not the clock's.

        A wall-clock build time would be the single field that makes two builds
        of the same commit differ -- it would land in the manifest, the manifest
        would land in the zip, and the published checksum would change every run
        while nothing about the software did. So the artifact takes its date from
        its source, which is what a reader of the manifest actually wants to know
        anyway ("what is in this?" rather than "when did a machine run cargo?").

        SOURCE_DATE_EPOCH is honored first: it is the reproducible-builds
        convention, and it is what a release pipeline reaching for a fixed date
        will already be setting. Otherwise the commit date. If git cannot say
        (no repo, no commits), the value is "unknown" rather than a fabricated
        time -- an artifact built outside a repo has no source date, and saying
        so is better than inventing one that looks authoritative.
        """
        epoch = (self.environ if self.environ is not None else os.environ).get(
            "SOURCE_DATE_EPOCH"
        )
        if epoch and epoch.strip().isdigit():
            return datetime.fromtimestamp(int(epoch.strip()), tz=UTC).isoformat()
        code, output = self.runner(
            ("git", "log", "-1", "--format=%cI"), self.repo_root, None
        )
        stamped = output.strip()
        return stamped if code == 0 and stamped else "unknown"

    def toolchain_info(self) -> dict:
        versions = {}
        for name, argv in {
            "rustc": ("rustc", "--version"),
            "cargo": ("cargo", "--version"),
            "swift": ("swift", "--version"),
        }.items():
            code, output = self.runner(argv, self.repo_root, None)
            lines = output.strip().splitlines()
            versions[name] = lines[0].strip() if code == 0 and lines else "unavailable"
        versions["uniffi"] = self.uniffi_version()
        return versions

    def uniffi_version(self) -> str:
        """The exact uniffi version the library links.

        Read from the lockfile via cargo, not from Cargo.toml's requirement: the
        requirement is a range and the generator/runtime pair is a toolchain
        contract (crates/gramdrive-ffi/README.md), so the resolved version is
        what a reader needs to reproduce the bindings.
        """
        code, output = self.runner(
            ("cargo", "metadata", "--format-version", "1", "--locked"), self.repo_root, None
        )
        if code != 0:
            return "unavailable"
        try:
            packages = json.loads(output)["packages"]
        except (json.JSONDecodeError, KeyError):
            return "unavailable"
        for package in packages:
            if package.get("name") == "uniffi":
                return str(package.get("version", "unavailable"))
        return "unavailable"

    def crate_version(self) -> str:
        code, output = self.runner(
            ("cargo", "metadata", "--format-version", "1", "--locked", "--no-deps"),
            self.repo_root,
            None,
        )
        if code != 0:
            return "unavailable"
        try:
            packages = json.loads(output)["packages"]
        except (json.JSONDecodeError, KeyError):
            return "unavailable"
        for package in packages:
            if package.get("name") == FFI_CRATE:
                return str(package.get("version", "unavailable"))
        return "unavailable"

    # -- steps -----------------------------------------------------------

    @property
    def target_dir(self) -> Path:
        """Packaging's own target directory, wiped before each shipped build."""
        return self.out_dir / TARGET_DIR_NAME

    def build_slices(self, extra_triples: Sequence[str] = ()) -> dict[str, Path]:
        """Build the shipped library for every slice, from a clean target dir.

        `extra_triples` builds additional host-test twins of the same source in
        the same clean target directory (--host-test-slice); they are staged for
        CI execution only and never enter the shipped slice list.

        The wipe is load-bearing and was measured, not assumed. The shipped
        library's bytes depend on what else was built in its target directory
        beforehand: at one fixed path, a build reusing the repo's target/ (547
        dependency artifacts, an incremental/ directory, a stale rlib and dylib
        from earlier plain `cargo build` runs) produced bab48d50, while the same
        source at the same path with a fresh target directory produced 110b1b9a
        -- which is also what every clean build produces at every other path.
        The delta is confined to LLVM's .llvm.* local-symbol suffixes, which
        thin LTO assigns and --remap-path-prefix does not reach.

        So the artifact gets a target directory of its own, wiped first, and the
        repo's target/ is left to the debug loop that owns it. `cargo clean -p`
        is not an alternative: it drops the named crate's artifacts and keeps
        every dependency, which is precisely the state that moves the bytes.

        Cost is a full release build per run; --skip-verify does not avoid it,
        because an artifact built from an unknown starting state is not the
        artifact this pipeline claims to produce.
        """
        if self.target_dir.exists():
            shutil.rmtree(self.target_dir)
        env = build_env(self.repo_root, self.target_dir, self.environ)
        self.build_record = BuildRecord(
            clean_target_dir=not self.target_dir.exists(),
            remapped_to=remapped_prefixes(env),
        )
        built: dict[str, Path] = {}
        extra = [triple for triple in extra_triples if triple not in {s.triple for s in SLICES}]
        labeled = [(entry.label, entry.triple) for entry in SLICES]
        labeled += [(f"host-test-{triple}", triple) for triple in extra]
        for label, triple in labeled:
            self.run(f"cargo-{label}", cargo_staticlib_argv(triple), env=env)
            library = self.target_dir / triple / "release" / f"{LIB_STEM}.a"
            if not library.is_file():
                raise StepFailed(
                    f"build for {triple} reported success but produced no "
                    f"{library}; the crate-type override may have changed"
                )
            built[triple] = library
        return built

    def generate_bindings(self, library: Path, out_dir: Path) -> None:
        """Generate the Swift bindings from the shipped library.

        Deliberately runs without build_env, so the generator builds in the
        repo's own target directory: it is a host tool whose output is text
        derived from `library`, nothing of it ships, and keeping its
        `--features bindgen` build (which pulls in uniffi's CLI machinery) out
        of packaging's target directory is what keeps that directory
        single-purpose and its contents predictable.
        """
        if out_dir.exists():
            shutil.rmtree(out_dir)
        out_dir.mkdir(parents=True)
        self.run("uniffi-bindgen", bindgen_argv(library, out_dir))
        expected = [f"{SWIFT_MODULE}.swift", f"{FFI_MODULE}.h", f"{FFI_MODULE}.modulemap"]
        missing = [name for name in expected if not (out_dir / name).is_file()]
        if missing:
            raise StepFailed(
                f"uniffi-bindgen did not produce {', '.join(missing)} in {out_dir}; "
                f"module naming is pinned in crates/{FFI_CRATE}/uniffi.toml"
            )

    def create_xcframework(
        self,
        libraries: dict[str, Path],
        bindings_dir: Path,
        artifact_dir: Path,
        host_test_lib: Path | None = None,
    ) -> Path:
        """Assemble the XCFramework from each slice's staticlib plus headers.

        With `host_test_lib` (--host-test-slice), the shipped macOS slice and
        the host-arch twin are lipo'd into one universal library first:
        xcodebuild refuses two separate -library entries for the same platform,
        and a universal archive is exactly the shape that lets the consumer
        verifier and the apple test suite link and *run* on the CI host while
        an arm64 consumer still links the shipped slice out of the same file.
        """
        headers_root = self.out_dir / "headers"
        if headers_root.exists():
            shutil.rmtree(headers_root)
        headers_root.mkdir(parents=True)
        shutil.copy2(bindings_dir / f"{FFI_MODULE}.h", headers_root / f"{FFI_MODULE}.h")
        # Clang requires the modulemap to be named module.modulemap inside a
        # framework's headers directory; uniffi names it after the module.
        shutil.copy2(bindings_dir / f"{FFI_MODULE}.modulemap", headers_root / "module.modulemap")

        output = artifact_dir / XCFRAMEWORK_NAME
        if output.exists():
            shutil.rmtree(output)
        argv: list[str] = ["xcodebuild", "-create-xcframework"]
        if host_test_lib is not None:
            if len(SLICES) != 1:
                raise StepFailed(
                    "--host-test-slice supports exactly one shipped macOS slice; "
                    "revisit the lipo step when SLICES grows platforms"
                )
            universal_dir = self.out_dir / "universal"
            if universal_dir.exists():
                shutil.rmtree(universal_dir)
            universal_dir.mkdir(parents=True)
            universal = universal_dir / f"{LIB_STEM}.a"
            self.run(
                "lipo-host-test",
                (
                    "lipo",
                    "-create",
                    str(libraries[SLICES[0].triple]),
                    str(host_test_lib),
                    "-output",
                    str(universal),
                ),
            )
            argv += ["-library", str(universal), "-headers", str(headers_root)]
        else:
            for entry in SLICES:
                argv += ["-library", str(libraries[entry.triple]), "-headers", str(headers_root)]
        argv += ["-output", str(output)]
        self.run("xcframework", argv)
        if not (output / "Info.plist").is_file():
            raise StepFailed(f"xcodebuild produced no Info.plist in {output}")
        return output

    def stage_artifact(
        self, bindings_dir: Path, artifact_dir: Path, tdjson_lib: Path | None = None
    ) -> None:
        sources = artifact_dir / "Sources" / SWIFT_MODULE
        sources.mkdir(parents=True, exist_ok=True)
        shutil.copy2(bindings_dir / f"{SWIFT_MODULE}.swift", sources / f"{SWIFT_MODULE}.swift")
        (artifact_dir / "Package.swift").write_text(
            artifact_package_swift(tdjson=tdjson_lib is not None), encoding="utf-8"
        )
        if tdjson_lib is not None:
            # Stage the runtime library beside the staticlib that references
            # it, with an absolute install name: local consumers (the
            # verifier, smokes, the apple test suite) load it with no rpath
            # story of their own; the app bundle rewrites to @rpath when it
            # embeds a copy.
            lib_dir = artifact_dir / "lib"
            lib_dir.mkdir(parents=True, exist_ok=True)
            staged = lib_dir / tdjson_lib.name
            shutil.copy2(tdjson_lib, staged)
            staged.chmod(0o644)
            self.run(
                "tdjson-install-name",
                ("install_name_tool", "-id", str(staged), str(staged)),
            )

    def verify_with_swift(
        self, consumer_dir: Path, library_path: Path | None = None
    ) -> dict:
        """Build and run the minimal Swift package against the staged artifact.

        This is the acceptance test for the artifact, so it uses SwiftPM and a
        real package dependency rather than a hand-built compiler command: the
        thing being proven is that a native host can consume what we ship.
        `library_path` (the tdjson staging) reaches ld64 through LIBRARY_PATH;
        at run time the dylib resolves by its absolute staged install name.
        """
        env = self.consumer_env(library_path)
        self.run(
            "swift-build",
            ("swift", "build", "--package-path", str(consumer_dir), "-c", "release"),
            env=env,
        )
        output = self.run(
            "swift-verify",
            ("swift", "run", "--package-path", str(consumer_dir), "-c", "release", "GramDriveVerify"),
            env=env,
        )
        return parse_verifier_report(output)

    def consumer_env(self, library_path: Path | None) -> dict[str, str] | None:
        if library_path is None:
            return None
        env = dict(self.environ if self.environ is not None else os.environ)
        env["LIBRARY_PATH"] = str(library_path)
        return env

    def cross_link_consumer(
        self, consumer_dir: Path, library_path: Path | None = None
    ) -> None:
        """Cross-build the consumer for the shipped arch, without running it.

        The degraded verifier for a host that cannot execute the shipped slice
        (x86_64 CI staging an arm64-only artifact, TASK-260719-1dwaj8): a real
        SwiftPM resolve and link against the staged artifact still proves the
        package shape and the archive's linkability for the shipped target; what
        it cannot prove is runtime behavior, so the caller records the mode and
        leaves the contract version unverified rather than inventing one.
        """
        arch = swift_arch(SLICES[0].triple)
        self.run(
            "swift-build",
            (
                "swift",
                "build",
                "--package-path",
                str(consumer_dir),
                "-c",
                "release",
                "--arch",
                arch,
            ),
            env=self.consumer_env(library_path),
        )


def prepare_consumer(repo_root: Path, out_dir: Path) -> Path:
    """Copy the checked-in verifier package next to the staged artifact.

    Copied rather than built in place: `.scripts/` is source, and a SwiftPM
    build drops .build/ and Package.resolved into whatever directory it runs in.
    The package's `../GramDriveCore` dependency resolves because of this layout.

    The copy excludes exactly what that build would have left behind. Copying it
    protects `.scripts/` from the build, but without the exclusion nothing
    protects the build from `.scripts/`: a stale .build/ from someone running
    `swift build` there by hand would be carried into the tree that is supposed
    to be the artifact's acceptance test, and stale SwiftPM state in the test
    that proves the artifact is a false pass waiting to happen.
    """
    source = repo_root / ".scripts" / "packaging" / "swift-consumer"
    consumer_dir = out_dir / "consumer"
    if consumer_dir.exists():
        shutil.rmtree(consumer_dir)
    shutil.copytree(source, consumer_dir, ignore=shutil.ignore_patterns(*COPY_EXCLUDES))
    return consumer_dir


def stage_build_inputs(repo_root: Path, dest: Path) -> None:
    """Copy the shipped build's inputs to `dest`, so it can be built there.

    Used by --check-reproducible to build the same source at a second path. The
    input list is explicit (BUILD_INPUTS) rather than "the whole repo minus
    ignores": the check is only meaningful if what it builds is what ships, and
    an explicit list makes a missing input a loud build failure at the staged
    path instead of a quiet difference in what was compared.
    """
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    for name in BUILD_INPUTS:
        source = repo_root / name
        if not source.exists():
            raise StepFailed(
                f"build input {name!r} is missing from {repo_root}; "
                f"--check-reproducible cannot build the shipped source elsewhere"
            )
        if source.is_dir():
            shutil.copytree(
                source, dest / name, ignore=shutil.ignore_patterns(*COPY_EXCLUDES)
            )
        else:
            shutil.copy2(source, dest / name)


def package(
    repo_root: Path,
    *,
    out_dir: Path,
    skip_verify: bool = False,
    host_test_slice: bool = False,
    host_machine: str | None = None,
    runner: Runner = default_runner,
    echo: Callable[[str], None] = print,
    environ: dict[str, str] | None = None,
) -> dict:
    """Run the whole pipeline and return the manifest.

    `host_machine` exists for tests; the default is the real host. It drives
    two decisions recorded in the manifest: whether --host-test-slice builds a
    twin (a host already in SLICES needs none) and whether the verifier can
    execute the staged slice (`native-run`) or only cross-link it.
    """
    packager = Packager(repo_root, out_dir, runner=runner, echo=echo, environ=environ)

    artifact_dir = out_dir / ARTIFACT_DIR_NAME
    if artifact_dir.exists():
        shutil.rmtree(artifact_dir)
    artifact_dir.mkdir(parents=True)
    bindings_dir = out_dir / "bindings"

    machine = host_machine if host_machine is not None else platform.machine()
    host = HOST_TRIPLES.get(machine)
    shipped_triples = {entry.triple for entry in SLICES}
    extra_triples: tuple[str, ...] = ()
    if host_test_slice:
        if host is None:
            raise StepFailed(
                f"--host-test-slice: unknown host architecture {machine!r}; "
                f"known: {', '.join(sorted(HOST_TRIPLES))}"
            )
        if host not in shipped_triples:
            extra_triples = (host,)
        else:
            echo(f"--- host-test-slice: host {machine} already ships; nothing to add")

    # The env-gated tdjson staging (BUG-260720-3i74u1): with the artifact
    # dir set, the crates' build scripts link the real tdjson (the same gate
    # `make tdjson-smoke` uses) and the runtime library is staged into the
    # artifact. Unset -- every hermetic gate run -- nothing here changes.
    tdlib_env = (environ if environ is not None else os.environ).get(
        "GRAMDRIVE_TDLIB_ARTIFACT_DIR"
    )
    tdjson_lib: Path | None = None
    if tdlib_env:
        tdjson_lib = Path(tdlib_env) / "lib" / "libtdjson.dylib"
        if not tdjson_lib.is_file():
            raise StepFailed(
                f"GRAMDRIVE_TDLIB_ARTIFACT_DIR is set but {tdjson_lib} does not "
                "exist; run `make tdlib` first"
            )

    libraries = packager.build_slices(extra_triples)
    # Bindings come from the release staticlib of the first slice: every slice
    # is the same source and the same contract, and UniFFI's checksums are over
    # the API, not the architecture.
    packager.generate_bindings(libraries[SLICES[0].triple], bindings_dir)
    packager.stage_artifact(bindings_dir, artifact_dir, tdjson_lib=tdjson_lib)
    host_test_lib = libraries[extra_triples[0]] if extra_triples else None
    packager.create_xcframework(
        libraries, bindings_dir, artifact_dir, host_test_lib=host_test_lib
    )

    # The verifier can execute only if the staged archive carries a slice this
    # host runs: natively shipped, or the --host-test-slice twin.
    staged_triples = shipped_triples | set(extra_triples)
    contract_version = "unverified"
    verify_mode = "skipped"
    if skip_verify:
        echo("--- swift-verify: SKIPPED (--skip-verify); contract version unverified")
    else:
        consumer_dir = prepare_consumer(repo_root, out_dir)
        staged_lib_dir = (artifact_dir / "lib") if tdjson_lib is not None else None
        if host in staged_triples:
            report = packager.verify_with_swift(consumer_dir, library_path=staged_lib_dir)
            contract_version = str(report["contract_version"])
            verify_mode = "native-run"
            echo(f"    verifier: contract {contract_version}, probe {report}")
        else:
            packager.cross_link_consumer(consumer_dir, library_path=staged_lib_dir)
            verify_mode = "cross-link-only"
            echo(
                f"--- swift-verify: cross-link only (host {machine} cannot execute "
                f"{SLICES[0].triple}); contract version unverified. The runtime "
                "probe for this commit is native-ci's apple-build-test leg."
            )

    slices = [
        {
            "triple": entry.triple,
            "label": entry.label,
            "staticlib_bytes": libraries[entry.triple].stat().st_size,
        }
        for entry in SLICES
    ]
    host_test_record = None
    if extra_triples:
        host_test_record = {
            "triple": extra_triples[0],
            "staticlib_bytes": libraries[extra_triples[0]].stat().st_size,
            "reason": (
                "CI host cannot execute the shipped slice; twin built from the "
                "same source so the verifier and the apple suite run natively. "
                "Test staging only -- never shipped (TASK-260719-1dwaj8)."
            ),
        }
    manifest = build_manifest(
        contract_version=contract_version,
        crate_version=packager.crate_version(),
        git=packager.git_info(),
        toolchain=packager.toolchain_info(),
        slices=slices,
        source_date=packager.source_date(),
        reproducible=reproducibility_record(packager.build_record),
        verify_mode=verify_mode,
        host_test_slice=host_test_record,
    )

    manifest["tdjson"] = {
        "linked": tdjson_lib is not None,
        **(
            {"library_sha256": sha256_file(artifact_dir / "lib" / tdjson_lib.name)}
            if tdjson_lib is not None
            else {}
        ),
    }

    # README and manifest go inside the artifact before it is checksummed, so
    # the checksums cover everything that ships.
    (artifact_dir / "README.md").write_text(render_artifact_readme(manifest), encoding="utf-8")
    (artifact_dir / MANIFEST_NAME).write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )

    zip_name = f"{SWIFT_MODULE}-{contract_version}.zip"
    zip_path = out_dir / zip_name
    write_deterministic_zip(artifact_dir, zip_path, prefix=SWIFT_MODULE)

    checksums = checksum_tree(artifact_dir)
    checksums[f"../{zip_name}"] = sha256_file(zip_path)
    (out_dir / "CHECKSUMS.sha256").write_text(format_checksums(checksums), encoding="utf-8")

    manifest["sizes"] = {
        "artifact_bytes": tree_size(artifact_dir),
        "xcframework_bytes": tree_size(artifact_dir / XCFRAMEWORK_NAME),
        "zip_bytes": zip_path.stat().st_size,
    }
    manifest["checksums"] = checksums
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    echo("")
    echo(f"artifact:     {artifact_dir}")
    echo(f"xcframework:  {tree_size(artifact_dir / XCFRAMEWORK_NAME):,} bytes")
    echo(f"artifact:     {manifest['sizes']['artifact_bytes']:,} bytes")
    echo(f"zip:          {zip_path} ({manifest['sizes']['zip_bytes']:,} bytes)")
    echo(f"zip sha256:   {checksums[f'../{zip_name}']}")
    echo(f"contract:     {contract_version}  commit: {manifest['git']['describe']}")
    return manifest


#: The two paths --check-reproducible builds at. They differ in length and in
#: name because the claim is that neither matters; two paths of the same shape
#: would be a weaker test for no saving.
CHECK_PATH_NAMES: tuple[str, ...] = ("a", "b-a-considerably-longer-checkout-directory")


def check_reproducible(
    repo_root: Path,
    *,
    out_dir: Path,
    echo: Callable[[str], None] = print,
    runner: Runner = default_runner,
    environ=None,
) -> int:
    """Build the shipped library at two different paths and compare the bytes.

    A claim is worth exactly as much as the check behind it, and this check
    exists to falsify the manifest's `path_independent`. That requires varying
    the path: an earlier version of this function built twice at the *same*
    path, which tests determinism -- real, but not the property asserted -- and
    structurally could not observe the axis it was supposed to cover.

    Both builds go through the same procedure the shipped artifact does
    (stage_build_inputs, then build_env + cargo_staticlib_argv into a fresh
    target directory), because a check whose procedure differs from the
    shipped build's does not cover the shipped artifact -- it covers some other
    build that happens to live in the same file.
    """
    root = out_dir / "reproducible"
    if root.exists():
        shutil.rmtree(root)

    digests: list[str] = []
    for index, name in enumerate(CHECK_PATH_NAMES, start=1):
        path = root / name
        stage_build_inputs(repo_root, path)
        # repo_root is the staged path: it is what gets remapped, and it is the
        # directory cargo runs in.
        packager = Packager(path, root / "logs", runner=runner, echo=echo, environ=environ)
        target_dir = path / TARGET_DIR_NAME
        env = build_env(path, target_dir, environ)
        packager.run(f"build-{index}", cargo_staticlib_argv(SLICES[0].triple), env=env)
        library = target_dir / SLICES[0].triple / "release" / f"{LIB_STEM}.a"
        if not library.is_file():
            echo(f"REPRODUCIBILITY CHECK FAILED: no library at {library}")
            return EXIT_FAILED
        digest = sha256_file(library)
        digests.append(digest)
        echo(f"    {path}: {digest}")
        # The library is measured; the target directory is ~200 MB of nothing
        # anyone will read.
        shutil.rmtree(target_dir)

    if len(set(digests)) != 1:
        echo(
            "REPRODUCIBILITY FAILED: the same source built at two paths produced "
            "different bytes, so the manifest's path_independent is false.\n"
            f"  staged trees kept at {root} for diffing"
        )
        return EXIT_FAILED
    shutil.rmtree(root)
    echo(f"REPRODUCIBLE at {len(digests)} paths: {digests[0]}")
    return EXIT_OK


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Package the GramDrive core for native consumers.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=None,
        help=f"where to stage artifacts (default: {OUT_ROOT})",
    )
    parser.add_argument(
        "--skip-verify",
        action="store_true",
        help=(
            "skip the Swift consumer. Leaves the contract version unverified, "
            "so the artifact is not release-grade; for iterating on the pipeline"
        ),
    )
    parser.add_argument(
        "--host-test-slice",
        action="store_true",
        help=(
            "additionally build a host-arch twin and lipo it into the staged "
            "slice, so the verifier and the apple suite can execute on a CI "
            "host that cannot run the shipped arch. Test staging only; the "
            "manifest and README record it and release never passes this"
        ),
    )
    parser.add_argument(
        "--check-reproducible",
        action="store_true",
        help=(
            "build the shipped library at two different paths from clean and "
            "compare bytes, then exit"
        ),
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(list(argv) if argv is not None else None)

    repo_root = args.repo_root.resolve()
    if sys.platform != "darwin":
        print(
            "ERROR: Apple artifacts require macOS (xcodebuild, swift). POL-5 makes "
            "macOS arm64 the v1 target; Windows/Linux hosts consume the crate directly.",
            file=sys.stderr,
        )
        return EXIT_CANNOT_START

    out_dir = (args.out_dir or (repo_root / OUT_ROOT)).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    if args.check_reproducible:
        try:
            return check_reproducible(repo_root, out_dir=out_dir)
        except StepFailed as failure:
            print(f"\nREPRODUCIBILITY CHECK FAILED\n{failure}", file=sys.stderr)
            return EXIT_FAILED

    try:
        package(
            repo_root,
            out_dir=out_dir,
            skip_verify=args.skip_verify,
            host_test_slice=args.host_test_slice,
        )
    except StepFailed as failure:
        print(f"\nPACKAGING FAILED\n{failure}", file=sys.stderr)
        return EXIT_FAILED
    print("\nPACKAGING PASSED")
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
