#!/usr/bin/env python3
"""Build the pinned TDLib tdjson artifact GramDrive's local source links against.

The local Telegram DriveSource (EPIC-260715-2ptb18, DEC-004) talks to Telegram
through TDLib's C JSON interface (tdjson). This script owns *what tdjson artifact
we ship and how it is attributable to a source commit*: the pinned TDLib
revision, the dependency and compiler policy, the produced library + headers +
license, and the version metadata and checksums that make the artifact
attributable to a commit (NFR-052, mirrors .scripts/packaging/build_core_artifacts.py).

    python3 .scripts/tdlib/build_tdlib.py            # fetch, build, stage, smoke
    python3 .scripts/tdlib/build_tdlib.py --skip-smoke
    python3 .scripts/tdlib/build_tdlib.py --fetch-only
    python3 .scripts/tdlib/build_tdlib.py --verify   # rebuild clean, compare bytes

What it produces, under .temp/tdlib/ (gitignored):

    src/                          TDLib checkout, pinned to TDLIB_COMMIT (cached)
    build/                        cmake build tree (cached; rebuilds are incremental)
    out/                          the staged, self-describing artifact:
      lib/libtdjson.dylib         the shared C JSON client library
      include/td/telegram/*.h     the public C headers a consumer compiles against
      LICENSE_1_0.txt             TDLib's Boost Software License 1.0 (POL-6)
      manifest.json               pin, version, toolchain, sizes, checksums, linkage
      CHECKSUMS.sha256            sha256 of every shipped file (`shasum -c` format)

Why a pinned from-source build, not a downloaded binary: no vendor publishes a
notarizable macOS-arm64 tdjson we can attribute to a commit, and POL-6 needs the
license provenance recorded at the point the bytes are produced. The pin is a
commit hash (TDLib tags rarely; the community pins commits), so the source is
immutable and the smoke test reads the *runtime* version out of the built
library rather than trusting a number parsed from source.

Reproducibility posture (honest, per NFR-052): the guarantee is
*attributability* -- pinned source commit + recorded toolchain + checksums, so
any consumer can tie the bytes to inputs. Byte-identical rebuild is pursued
same-machine (Release build, `ZERO_AR_DATE`, `-ffile-prefix-map`, ld64's
content-hashed LC_UUID) and checkable with `--verify`, which rebuilds from a
clean tree and compares the recorded library checksum. Cross-machine identical
bytes depend on identical clang/OpenSSL and are best-effort, not claimed.

Requires: git, cmake, gperf, a C++ toolchain (Xcode clang) and OpenSSL on a
macOS arm64 host. POL-5/DEC-017 make macOS 14+ arm64 the entire v1 support
matrix; on any other host the script exits 2 with that reason rather than
producing a partial artifact. Python 3.11+, stdlib only.

Exit codes: 0 built, 1 a step failed, 2 the run could not start.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_CANNOT_START = 2

OUT_ROOT = Path(".temp") / "tdlib"

# -- The pin. Exactly one place. --------------------------------------------
#
# TDLib does not cut releases on a schedule (last git tag v1.8.0 is from 2022
# while the library is on 1.8.x well past it), so the ecosystem pins commits.
# This is an immutable commit hash resolved from tdlib/td master on 2026-07-17;
# bumping TDLib means editing this one constant and re-running the build.
TDLIB_REPO = "https://github.com/tdlib/td.git"
TDLIB_COMMIT = "022d60202e446ad1287b9fb68e687c8a0760788b"

# The shipped target. POL-5/DEC-017: macOS 14+ arm64 is the whole v1 matrix.
# Other platforms are documented and deferred in README.md, same posture as
# the core packaging pipeline -- a stubbed cross-build here would be a path
# nothing runs and nothing checks. Adding one is adding a Target row, not a
# rewrite.
TARGET_LABEL = "macos-arm64"
TARGET_ARCH = "arm64"
# The macOS floor the artifact claims (POL-5). Passed to cmake so the objects
# in the dylib carry the same deployment target the native host declares; a
# mismatch surfaces as linker warnings in the consuming app.
MACOSX_DEPLOYMENT_TARGET = "14.0"

# What we build and stage. `tdjson` is the SHARED library target: it links
# every TDLib sub-library (tdcore, tdactor, tdnet, tddb, tdutils, ...) into
# itself and exports only the C JSON interface, so the single dylib is the
# whole client. The public headers a consumer needs are the two source headers
# plus the export-macro header cmake generates at build time.
TDJSON_TARGET = "tdjson"
DYLIB_NAME = "libtdjson.dylib"
PUBLIC_HEADERS = ("td_json_client.h", "td_log.h")
GENERATED_HEADER = "tdjson_export.h"
HEADER_INSTALL_SUBDIR = Path("include") / "td" / "telegram"
LICENSE_SRC_NAME = "LICENSE_1_0.txt"
LICENSE_ID = "BSL-1.0"

MANIFEST_NAME = "manifest.json"
CHECKSUMS_NAME = "CHECKSUMS.sha256"
SCHEMA_VERSION = "tdlib-artifact/1"

# The dylib's install name. `@rpath` hands load-path control to the consumer
# (an rpath at its link step) rather than baking this machine's absolute path
# into the artifact -- the same reason the core zip records no `<from>` paths.
DYLIB_INSTALL_NAME = f"@rpath/{DYLIB_NAME}"

# The standalone Cargo project that proves the artifact links and runs. It has
# its own [workspace] table so it never joins the gramdrive workspace: TDLib
# linkage must not leak into `cargo build --workspace` / `make check`, which run
# on machines that never built this artifact (mirrors the reserved
# gramdrive-source-tdjson isolation, LOGBOOK 2026-07-17).
SMOKE_CRATE_DIR = Path(".scripts") / "tdlib" / "link-smoke"
SMOKE_ARTIFACT_ENV = "GRAMDRIVE_TDLIB_ARTIFACT_DIR"


Runner = Callable[[Sequence[str], Path, "dict[str, str] | None"], "tuple[int, str]"]


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
    """A build step failed; carries what to print and nothing else."""


@dataclass(frozen=True)
class BuildRecord:
    """What the build actually did, so the manifest reports rather than asserts.

    `path_independent` in the manifest is derived from these, not written as a
    literal, so an artifact cannot claim a reproducibility property of a build
    that did not happen.
    """

    #: Whether the build tree was wiped before this build (incremental if not).
    clean_build_tree: bool
    #: The prefix source paths were remapped to (never the local path it was
    #: remapped *from* -- that would put this machine's path back in the metadata).
    remapped_to: str
    #: cmake build type.
    build_type: str
    #: Whether ar/libtool timestamps were zeroed (ZERO_AR_DATE).
    deterministic_archives: bool


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def checksum_tree(root: Path, *, exclude: Sequence[str] = ()) -> dict[str, str]:
    """sha256 of every file under root, keyed by POSIX path relative to it.

    Sorted so the output is stable across filesystems: a checksum file whose
    line order depends on readdir order is a diff that lies. `exclude` drops
    files by relative POSIX name -- the checksum file and manifest cannot list
    themselves, since writing either would change what it claims.
    """
    excluded = set(exclude)
    return {
        rel: sha256_file(path)
        for path in sorted(root.rglob("*"))
        if path.is_file()
        and not path.is_symlink()
        and (rel := path.relative_to(root).as_posix()) not in excluded
    }


def format_checksums(checksums: dict[str, str]) -> str:
    """Render checksums in `shasum -a 256 -c` format."""
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items()))


def tree_size(root: Path) -> int:
    return sum(
        p.stat().st_size for p in root.rglob("*") if p.is_file() and not p.is_symlink()
    )


class TdlibBuilder:
    """Fetch, build, stage and describe the pinned tdjson artifact."""

    def __init__(
        self,
        repo_root: Path,
        out_dir: Path,
        *,
        runner: Runner = default_runner,
        jobs: int | None = None,
        environ: dict[str, str] | None = None,
    ) -> None:
        self.repo_root = repo_root
        self.out_dir = out_dir
        self.runner = runner
        self.jobs = jobs or (os.cpu_count() or 4)
        self.environ = environ if environ is not None else dict(os.environ)

    # -- paths -----------------------------------------------------------

    @property
    def src_dir(self) -> Path:
        return self.out_dir / "src"

    @property
    def build_dir(self) -> Path:
        return self.out_dir / "build"

    @property
    def stage_dir(self) -> Path:
        return self.out_dir / "out"

    @property
    def lib_out(self) -> Path:
        return self.stage_dir / "lib" / DYLIB_NAME

    # -- small helpers ---------------------------------------------------

    def run(
        self,
        name: str,
        argv: Sequence[str],
        *,
        cwd: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> str:
        code, output = self.runner(argv, cwd or self.repo_root, env)
        if code != 0:
            raise StepFailed(f"{name} failed ({code}):\n{output}")
        return output

    def tool_version(self, argv: Sequence[str], line: int = 0) -> str:
        code, output = self.runner(argv, self.repo_root, None)
        lines = output.strip().splitlines()
        if code != 0 or not lines:
            return "unavailable"
        return lines[min(line, len(lines) - 1)].strip()

    # -- dependency discovery -------------------------------------------

    def openssl_root(self) -> Path:
        """Where OpenSSL lives, so cmake finds it without guessing.

        Homebrew keeps OpenSSL keg-only (off the default search path), so a
        cmake that does not get OPENSSL_ROOT_DIR either fails to find it or, on
        an Intel-era layout, finds the wrong one. `brew --prefix` is the
        supported way to resolve it; an explicit env override wins for a
        vendored or non-brew OpenSSL.
        """
        override = self.environ.get("OPENSSL_ROOT_DIR")
        if override:
            return Path(override)
        code, output = self.runner(("brew", "--prefix", "openssl@3"), self.repo_root, None)
        prefix = output.strip()
        if code != 0 or not prefix:
            raise StepFailed(
                "cannot locate OpenSSL: set OPENSSL_ROOT_DIR, or `brew install "
                "openssl@3`. TDLib links libcrypto/libssl and cmake needs the root."
            )
        return Path(prefix)

    def require_tools(self) -> None:
        """Fail early and specifically if a required build tool is missing."""
        needed = {
            "git": ("git", "--version"),
            "cmake": ("cmake", "--version"),
            "gperf": ("gperf", "--version"),
            "cc": ("cc", "--version"),
        }
        missing = [
            name
            for name, argv in needed.items()
            if self.runner(argv, self.repo_root, None)[0] != 0
        ]
        if missing:
            raise StepFailed(
                "missing build tools: "
                + ", ".join(missing)
                + ". Install Xcode command line tools and `brew install cmake gperf`."
            )

    # -- steps -----------------------------------------------------------

    def fetch(self) -> None:
        """Check out TDLib at the pinned commit, incrementally.

        A commit already checked out means no network: the existing tree's HEAD
        is compared to the pin and left alone if it matches, so a rerun is
        offline and instant. Otherwise a shallow fetch of exactly the pinned
        commit -- the whole history is not a build input, and fetching one
        commit keeps the clone small and the pin unambiguous.
        """
        git_dir = self.src_dir / ".git"
        if git_dir.is_dir():
            code, head = self.runner(
                ("git", "rev-parse", "HEAD"), self.src_dir, None
            )
            if code == 0 and head.strip() == TDLIB_COMMIT:
                return
        self.src_dir.mkdir(parents=True, exist_ok=True)
        if not git_dir.is_dir():
            self.run("git init", ("git", "init", "-q"), cwd=self.src_dir)
            self.run(
                "git remote add",
                ("git", "remote", "add", "origin", TDLIB_REPO),
                cwd=self.src_dir,
            )
        self.run(
            "git fetch",
            ("git", "fetch", "--depth", "1", "origin", TDLIB_COMMIT),
            cwd=self.src_dir,
        )
        self.run(
            "git checkout",
            ("git", "checkout", "-q", "--force", TDLIB_COMMIT),
            cwd=self.src_dir,
        )

    def tdlib_source_version(self) -> str:
        """The TDLib version declared in the checked-out CMakeLists.txt.

        Recorded alongside -- not instead of -- the runtime version the smoke
        test reads: this one ties the pin to a human-readable number without a
        build, the runtime one proves the built library actually implements it.
        """
        cmake_lists = self.src_dir / "CMakeLists.txt"
        try:
            text = cmake_lists.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return "unknown"
        match = re.search(
            r"project\s*\(\s*TDLib\s+VERSION\s+([0-9][0-9.]*)", text, re.IGNORECASE
        )
        return match.group(1) if match else "unknown"

    def configure_and_build(self, *, clean: bool) -> BuildRecord:
        """cmake configure + build only the tdjson target.

        Release build type keeps debug info (and thus embedded source paths) out
        of the objects, `-ffile-prefix-map` remaps what remains, and
        ZERO_AR_DATE zeroes archive timestamps -- the C++ analogue of the core
        pipeline's rustflag remap. Only `tdjson` is built: the CLI examples and
        the static sub-libraries are not shipped, and not building them keeps the
        tree small and the build short.
        """
        if clean and self.build_dir.exists():
            shutil.rmtree(self.build_dir)
        self.build_dir.mkdir(parents=True, exist_ok=True)

        remap_to = "/tdlib"
        openssl_root = self.openssl_root()
        # -ffile-prefix-map rewrites __FILE__, debug paths and NDEBUG-surviving
        # diagnostics from the build tree to a stable prefix; Release already
        # sets -DNDEBUG which drops most of them.
        prefix_map = f"-ffile-prefix-map={self.src_dir}={remap_to}"
        configure = [
            "cmake",
            "-S",
            str(self.src_dir),
            "-B",
            str(self.build_dir),
            "-DCMAKE_BUILD_TYPE=Release",
            f"-DCMAKE_OSX_ARCHITECTURES={TARGET_ARCH}",
            f"-DCMAKE_OSX_DEPLOYMENT_TARGET={MACOSX_DEPLOYMENT_TARGET}",
            f"-DOPENSSL_ROOT_DIR={openssl_root}",
            f"-DCMAKE_C_FLAGS={prefix_map}",
            f"-DCMAKE_CXX_FLAGS={prefix_map}",
            # Hand load-path control to the consumer instead of baking an
            # absolute path into the artifact.
            f"-DCMAKE_INSTALL_NAME_DIR=@rpath",
        ]
        # ZERO_AR_DATE makes Apple ar/libtool write zeroed timestamps, so a
        # byte-identical object produces a byte-identical archive.
        build_env = {**self.environ, "ZERO_AR_DATE": "1"}
        self.run("cmake configure", configure, env=build_env)
        self.run(
            "cmake build",
            (
                "cmake",
                "--build",
                str(self.build_dir),
                "--target",
                TDJSON_TARGET,
                "-j",
                str(self.jobs),
            ),
            env=build_env,
        )
        return BuildRecord(
            clean_build_tree=clean,
            remapped_to=remap_to,
            build_type="Release",
            deterministic_archives=True,
        )

    def stage(self, dest: Path | None = None) -> Path:
        """Collect the dylib, public headers and license into `dest` (default out/).

        The dylib's install name is normalized to @rpath here rather than
        trusted from the build: CMAKE_INSTALL_NAME_DIR sets it, and this is the
        belt-and-braces check that what ships is relocatable. Returns the staged
        library path so callers (the reproducibility check) can hash exactly the
        bytes that would ship. `dest` lets the check stage into a scratch tree
        instead of clobbering the canonical out/.
        """
        stage_dir = dest or self.stage_dir
        if stage_dir.exists():
            shutil.rmtree(stage_dir)
        lib_dir = stage_dir / "lib"
        header_dir = stage_dir / HEADER_INSTALL_SUBDIR
        lib_dir.mkdir(parents=True, exist_ok=True)
        header_dir.mkdir(parents=True, exist_ok=True)
        lib_out = lib_dir / DYLIB_NAME

        built = self.build_dir / DYLIB_NAME
        if not built.is_file():
            raise StepFailed(
                f"build reported success but {built} is missing -- the tdjson "
                f"target did not produce a dylib"
            )
        shutil.copy2(built, lib_out)
        # Best-effort install-name normalization; harmless if already @rpath.
        self.runner(
            ("install_name_tool", "-id", DYLIB_INSTALL_NAME, str(lib_out)),
            self.repo_root,
            None,
        )

        for name in PUBLIC_HEADERS:
            src = self.src_dir / "td" / "telegram" / name
            if not src.is_file():
                raise StepFailed(f"expected public header missing: {src}")
            shutil.copy2(src, header_dir / name)
        generated = self._find_generated_header()
        shutil.copy2(generated, header_dir / GENERATED_HEADER)

        license_src = self.src_dir / LICENSE_SRC_NAME
        if not license_src.is_file():
            raise StepFailed(
                f"TDLib license file missing at {license_src}; POL-6 requires the "
                f"license be recorded with the artifact"
            )
        shutil.copy2(license_src, stage_dir / LICENSE_SRC_NAME)
        return lib_out

    def _find_generated_header(self) -> Path:
        """Locate the cmake-generated export header in the build tree."""
        matches = sorted(self.build_dir.rglob(GENERATED_HEADER))
        if not matches:
            raise StepFailed(
                f"generated header {GENERATED_HEADER} not found under "
                f"{self.build_dir}; the tdjson export header was not produced"
            )
        return matches[0]

    def library_linkage(self) -> list[str]:
        """The dylib's dynamic dependencies (`otool -L`), recorded in the manifest.

        A consumer needs to know the artifact expects OpenSSL and zlib at load
        time; recording it is cheaper than a surprise dyld failure in a native
        host. Each dependency is one tab-indented line; the first such line is
        the library's own install name, which is not a dependency and is dropped.
        """
        code, output = self.runner(("otool", "-L", str(self.lib_out)), self.repo_root, None)
        if code != 0:
            return ["unavailable"]
        entries = [
            line.split(" (", 1)[0].strip()
            for line in output.splitlines()
            if line.startswith("\t")
        ]
        return entries[1:] or ["none"]

    # -- provenance ------------------------------------------------------

    def git_info(self) -> dict:
        code, describe = self.runner(
            ("git", "describe", "--tags", "--always", "--dirty"), self.repo_root, None
        )
        _, commit = self.runner(("git", "rev-parse", "HEAD"), self.repo_root, None)
        status_code, status = self.runner(
            ("git", "status", "--porcelain"), self.repo_root, None
        )
        return {
            "describe": describe.strip() if code == 0 else "unknown",
            "commit": commit.strip() or None,
            "worktree_clean": (status.strip() == "") if status_code == 0 else None,
        }

    def source_date(self) -> str:
        """The date the artifact is stamped with: the source's, not the clock's.

        SOURCE_DATE_EPOCH first (the reproducible-builds convention a release
        pipeline already sets), else the gramdrive commit date. A wall-clock
        stamp is the one field that would make two builds of one commit differ.
        """
        epoch = self.environ.get("SOURCE_DATE_EPOCH")
        if epoch and epoch.strip().isdigit():
            return datetime.fromtimestamp(int(epoch.strip()), tz=UTC).isoformat()
        code, output = self.runner(
            ("git", "log", "-1", "--format=%cI"), self.repo_root, None
        )
        stamped = output.strip()
        return stamped if code == 0 and stamped else "unknown"

    def toolchain_info(self) -> dict:
        openssl_root = self.environ.get("OPENSSL_ROOT_DIR") or self._safe_openssl_prefix()
        return {
            "cmake": self.tool_version(("cmake", "--version")),
            "cc": self.tool_version(("cc", "--version")),
            "gperf": self.tool_version(("gperf", "--version")),
            "rustc": self.tool_version(("rustc", "--version")),
            "cargo": self.tool_version(("cargo", "--version")),
            "openssl_root": str(openssl_root) if openssl_root else "unavailable",
            "openssl": self._openssl_version(openssl_root),
            "zlib": self._zlib_version(),
        }

    def _safe_openssl_prefix(self) -> Path | None:
        try:
            return self.openssl_root()
        except StepFailed:
            return None

    def _openssl_version(self, root: Path | None) -> str:
        if root is None:
            return "unavailable"
        openssl_bin = Path(root) / "bin" / "openssl"
        return self.tool_version((str(openssl_bin), "version"))

    def _zlib_version(self) -> str:
        """The zlib version from the active SDK header (macOS ships zlib).

        Read from zlib.h's ZLIB_VERSION define rather than a linked library:
        it is the header the build compiles against.
        """
        code, sdk = self.runner(("xcrun", "--show-sdk-path"), self.repo_root, None)
        if code != 0 or not sdk.strip():
            return "unavailable"
        header = Path(sdk.strip()) / "usr" / "include" / "zlib.h"
        try:
            text = header.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return "unavailable"
        match = re.search(r'#\s*define\s+ZLIB_VERSION\s+"([^"]+)"', text)
        return match.group(1) if match else "unknown"

    # -- smoke -----------------------------------------------------------

    def smoke_version(self) -> str:
        """Build and run the Rust link-smoke bin; return the TDLib version it prints.

        This is the acceptance test and the source of the manifest's runtime
        version: a Rust binary that links libtdjson, calls the C JSON interface,
        and reads the version out of the running library. If it prints nothing
        parseable, the artifact is not consumable and the run fails.
        """
        crate_dir = self.repo_root / SMOKE_CRATE_DIR
        env = {**self.environ, SMOKE_ARTIFACT_ENV: str(self.stage_dir)}
        output = self.run(
            "smoke run",
            ("cargo", "run", "--quiet", "--release"),
            cwd=crate_dir,
            env=env,
        )
        version = parse_smoke_version(output)
        if version is None:
            raise StepFailed(
                "link-smoke ran but printed no parseable TDLib version:\n" + output
            )
        return version

    # -- manifest --------------------------------------------------------

    def build_manifest(
        self, record: BuildRecord, runtime_version: str | None
    ) -> dict:
        checksums = checksum_tree(
            self.stage_dir, exclude=(MANIFEST_NAME, CHECKSUMS_NAME)
        )
        return {
            "schema": SCHEMA_VERSION,
            "tool": "build_tdlib.py",
            "generated_for_source_date": self.source_date(),
            "gramdrive": self.git_info(),
            "tdlib": {
                "repo": TDLIB_REPO,
                "commit": TDLIB_COMMIT,
                "source_version": self.tdlib_source_version(),
                "runtime_version": runtime_version,
            },
            "target": {
                "label": TARGET_LABEL,
                "arch": TARGET_ARCH,
                "macosx_deployment_target": MACOSX_DEPLOYMENT_TARGET,
            },
            "license": {"id": LICENSE_ID, "file": LICENSE_SRC_NAME},
            "toolchain": self.toolchain_info(),
            "linkage": self.library_linkage(),
            "reproducibility": {
                "build_type": record.build_type,
                "clean_build_tree": record.clean_build_tree,
                "deterministic_archives": record.deterministic_archives,
                "path_independent": record.clean_build_tree
                and bool(record.remapped_to),
                "remapped_to": record.remapped_to,
            },
            "artifacts": {
                "total_bytes": tree_size(self.stage_dir),
                "files": {name: {"sha256": digest} for name, digest in checksums.items()},
                "library": {
                    "path": str(self.lib_out.relative_to(self.stage_dir).as_posix()),
                    "install_name": DYLIB_INSTALL_NAME,
                    "sha256": sha256_file(self.lib_out),
                    "bytes": self.lib_out.stat().st_size,
                },
            },
        }

    def write_manifest_and_checksums(self, manifest: dict) -> None:
        (self.stage_dir / MANIFEST_NAME).write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        checksums = checksum_tree(
            self.stage_dir, exclude=(MANIFEST_NAME, CHECKSUMS_NAME)
        )
        (self.stage_dir / CHECKSUMS_NAME).write_text(
            format_checksums(checksums), encoding="utf-8"
        )


def parse_smoke_version(output: str) -> str | None:
    """Extract the TDLib version the smoke bin printed on its `TDLib version:` line."""
    match = re.search(r"TDLib version:\s*([0-9][0-9A-Za-z.\-]*)", output)
    return match.group(1) if match else None


def build(
    repo_root: Path,
    *,
    out_dir: Path,
    runner: Runner = default_runner,
    jobs: int | None = None,
    skip_smoke: bool = False,
    fetch_only: bool = False,
    clean: bool = False,
) -> dict:
    builder = TdlibBuilder(repo_root, out_dir, runner=runner, jobs=jobs)
    builder.require_tools()
    builder.fetch()
    if fetch_only:
        return {"fetched": TDLIB_COMMIT}
    record = builder.configure_and_build(clean=clean)
    builder.stage()
    runtime_version = None if skip_smoke else builder.smoke_version()
    manifest = builder.build_manifest(record, runtime_version)
    builder.write_manifest_and_checksums(manifest)
    return manifest


def verify_reproducible(
    repo_root: Path, *, out_dir: Path, runner: Runner = default_runner, jobs: int | None = None
) -> int:
    """Build twice from a clean tree and compare the library's bytes.

    Reuses one source checkout (the source is pinned and immutable) but wipes
    the build tree before each build, because a reused build tree is the axis
    most likely to move the bytes. Same-machine reproducibility is what this
    can honestly assert; that is what CI caching depends on.

    Each build stages into its own scratch tree so the canonical out/ (and its
    manifest) is never clobbered: a check must not overwrite the artifact it
    exists to check.
    """
    builder = TdlibBuilder(repo_root, out_dir, runner=runner, jobs=jobs)
    builder.require_tools()
    builder.fetch()

    scratch = out_dir / "_verify"
    if scratch.exists():
        shutil.rmtree(scratch)

    builder.configure_and_build(clean=True)
    first = sha256_file(builder.stage(scratch / "a"))

    builder.configure_and_build(clean=True)
    second = sha256_file(builder.stage(scratch / "b"))

    shutil.rmtree(scratch, ignore_errors=True)

    if first != second:
        raise StepFailed(
            "library bytes differ between two clean builds of the same commit:\n"
            f"  build 1: {first}\n  build 2: {second}\n"
            "same-machine reproducibility does not hold; investigate before shipping"
        )
    print(f"REPRODUCIBLE: two clean builds of {TDLIB_COMMIT[:12]} agree\n  {DYLIB_NAME} sha256 {first}")
    return EXIT_OK


def host_supported() -> tuple[bool, str]:
    """Whether this host can build the v1 artifact (macOS arm64; POL-5/DEC-017)."""
    if sys.platform != "darwin":
        return False, (
            "TDLib artifact requires macOS (Apple clang, notarizable dylib). "
            "POL-5/DEC-017 make macOS arm64 the v1 target; Windows/Linux hosts "
            "consume the tdjson source through their own build."
        )
    machine = os.uname().machine
    if machine != "arm64":
        return False, (
            f"host arch is {machine}; POL-5/DEC-017 make arm64 the only v1 "
            "target and Intel macOS is explicitly out of scope"
        )
    return True, ""


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build the pinned TDLib tdjson artifact for GramDrive.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--out-dir", type=Path, default=None, help=f"where to stage (default: {OUT_ROOT})"
    )
    parser.add_argument("--jobs", type=int, default=None, help="parallel build jobs")
    parser.add_argument(
        "--skip-smoke",
        action="store_true",
        help="skip the Rust link-smoke; leaves the runtime version unverified",
    )
    parser.add_argument(
        "--fetch-only", action="store_true", help="check out the pin and stop"
    )
    parser.add_argument(
        "--clean", action="store_true", help="wipe the build tree before building"
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="build twice from a clean tree and compare library bytes, then exit",
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(list(argv) if argv is not None else None)

    repo_root = args.repo_root.resolve()
    ok, reason = host_supported()
    if not ok:
        print(f"ERROR: {reason}", file=sys.stderr)
        return EXIT_CANNOT_START

    out_dir = (args.out_dir or (repo_root / OUT_ROOT)).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    try:
        if args.verify:
            return verify_reproducible(repo_root, out_dir=out_dir, jobs=args.jobs)
        manifest = build(
            repo_root,
            out_dir=out_dir,
            jobs=args.jobs,
            skip_smoke=args.skip_smoke,
            fetch_only=args.fetch_only,
            clean=args.clean,
        )
    except StepFailed as failure:
        print(f"\nTDLIB BUILD FAILED\n{failure}", file=sys.stderr)
        return EXIT_FAILED

    if args.fetch_only:
        print(f"FETCHED TDLib {TDLIB_COMMIT}")
        return EXIT_OK
    version = manifest["tdlib"].get("runtime_version") or manifest["tdlib"]["source_version"]
    print(
        f"\nTDLIB BUILD PASSED\n"
        f"  tdlib {version} @ {TDLIB_COMMIT[:12]}\n"
        f"  {manifest['artifacts']['library']['path']} "
        f"{manifest['artifacts']['library']['bytes']} B\n"
        f"  staged in {out_dir / 'out'}"
    )
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
