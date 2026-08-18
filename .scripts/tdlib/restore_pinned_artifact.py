#!/usr/bin/env python3
"""Restore and verify the pinned arm64 TDLib artifact from runner-local cache.

The dedicated signing runner is x86_64 and cannot truthfully rebuild the
arm64/OpenSSL artifact. This helper implements the same cold-cache contract as
native-ci while making the integrity checks reusable by candidate-build.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BUILD_SCRIPT = SCRIPT_DIR / "build_tdlib.py"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
METADATA_FILES = {"manifest.json", "CHECKSUMS.sha256"}
EXIT_COLD_CACHE = 2


def _load_build_contract():
    spec = importlib.util.spec_from_file_location("pinned_build_tdlib", BUILD_SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load TDLib build contract from {BUILD_SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


build_contract = _load_build_contract()
FileRunner = Callable[[Sequence[str]], tuple[int, str]]


class ArtifactError(RuntimeError):
    """The cached artifact fails its pinned integrity contract."""


class ColdCacheError(ArtifactError):
    """The recipe-keyed runner-local cache has not been seeded."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def cache_key() -> str:
    return f"tdlib-{sha256_file(BUILD_SCRIPT)[:16]}"


def default_file_runner(argv: Sequence[str]) -> tuple[int, str]:
    try:
        result = subprocess.run(list(argv), capture_output=True, text=True)
    except FileNotFoundError:
        return 127, "file: not found"
    return result.returncode, result.stdout + result.stderr


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ArtifactError(message)


def artifact_files(root: Path) -> dict[str, Path]:
    files: dict[str, Path] = {}
    for path in sorted(root.rglob("*")):
        require(not path.is_symlink(), f"cached TDLib artifact contains a symlink: {path}")
        if path.is_file():
            name = path.relative_to(root).as_posix()
            if name not in METADATA_FILES:
                files[name] = path
    return files


def parse_checksums(path: Path) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ArtifactError(f"cannot read cached TDLib checksums: {error}") from error
    checksums: dict[str, str] = {}
    for number, line in enumerate(lines, 1):
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\0\r\n]+)", line)
        require(match is not None, f"malformed cached TDLib checksum line {number}")
        digest, name = match.groups()
        candidate = Path(name)
        require(
            not candidate.is_absolute()
            and ".." not in candidate.parts
            and name not in METADATA_FILES,
            f"unsafe cached TDLib checksum path: {name!r}",
        )
        require(name not in checksums, f"duplicate cached TDLib checksum path: {name!r}")
        checksums[name] = digest
    return checksums


def validate_artifact(
    root: Path, *, file_runner: FileRunner = default_file_runner
) -> dict:
    require(
        root.is_dir() and not root.is_symlink(),
        f"cached TDLib artifact is not a directory: {root}",
    )
    try:
        manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ArtifactError(f"cannot read cached TDLib manifest: {error}") from error
    require(isinstance(manifest, dict), "cached TDLib manifest is not an object")

    checksums = parse_checksums(root / "CHECKSUMS.sha256")
    files = artifact_files(root)
    require(
        set(checksums) == set(files),
        "cached TDLib checksum inventory does not exactly match its files",
    )
    for name, digest in checksums.items():
        require(
            sha256_file(files[name]) == digest,
            f"cached TDLib checksum mismatch: {name}",
        )

    require(
        manifest.get("schema") == build_contract.SCHEMA_VERSION,
        "cached TDLib manifest schema is not pinned",
    )
    require(
        manifest.get("tool") == "build_tdlib.py",
        "cached TDLib manifest tool is not build_tdlib.py",
    )
    gramdrive = manifest.get("gramdrive", {})
    require(
        COMMIT_RE.fullmatch(str(gramdrive.get("commit", ""))) is not None,
        "cached TDLib builder commit is missing",
    )
    require(
        gramdrive.get("worktree_clean") is True,
        "cached TDLib artifact came from a dirty builder worktree",
    )
    tdlib = manifest.get("tdlib", {})
    require(
        tdlib.get("repo") == build_contract.TDLIB_REPO,
        "cached TDLib repository is not pinned",
    )
    require(
        tdlib.get("commit") == build_contract.TDLIB_COMMIT,
        "cached TDLib commit is not pinned",
    )
    require(
        bool(tdlib.get("runtime_version")),
        "cached TDLib runtime probe result is missing",
    )
    target = manifest.get("target", {})
    require(
        target.get("label") == build_contract.TARGET_LABEL
        and target.get("arch") == build_contract.TARGET_ARCH
        and target.get("macosx_deployment_target")
        == build_contract.MACOSX_DEPLOYMENT_TARGET,
        "cached TDLib target is not the pinned macos-arm64 contract",
    )
    require(
        manifest.get("reproducibility", {}).get("clean_build_tree") is True,
        "cached TDLib was not built from a clean tree",
    )
    require(
        manifest.get("license", {}).get("id") == build_contract.LICENSE_ID,
        "cached TDLib license identity is missing",
    )

    records = manifest.get("artifacts", {}).get("files", {})
    require(isinstance(records, dict), "cached TDLib manifest file inventory is malformed")
    require(
        set(records) == set(checksums)
        and all(isinstance(record, dict) for record in records.values()),
        "cached TDLib manifest file inventory is not exact",
    )
    manifest_checksums = {
        name: record.get("sha256")
        for name, record in records.items()
        if isinstance(record, dict)
    }
    require(
        manifest_checksums == checksums,
        "cached TDLib manifest digest inventory does not match CHECKSUMS.sha256",
    )
    require(
        manifest.get("artifacts", {}).get("total_bytes")
        == sum(path.stat().st_size for path in files.values()),
        "cached TDLib total byte count disagrees with its inventory",
    )
    library_record = manifest.get("artifacts", {}).get("library", {})
    library_name = f"lib/{build_contract.DYLIB_NAME}"
    library = files.get(library_name)
    require(library is not None, f"cached TDLib library is missing: {library_name}")
    require(
        library_record.get("path") == library_name,
        "cached TDLib library path is not pinned",
    )
    require(
        library_record.get("install_name") == build_contract.DYLIB_INSTALL_NAME,
        "cached TDLib library install name is not pinned",
    )
    require(
        library_record.get("sha256") == checksums[library_name],
        "cached TDLib library digest record disagrees",
    )
    require(
        library_record.get("bytes") == library.stat().st_size,
        "cached TDLib library size record disagrees",
    )
    require(
        SHA256_RE.fullmatch(str(library_record.get("sha256", ""))) is not None,
        "cached TDLib library digest is malformed",
    )

    code, file_output = file_runner(("file", str(library)))
    require(
        code == 0,
        f"file(1) could not inspect cached TDLib library: {file_output.strip()}",
    )
    require(
        "Mach-O" in file_output and "arm64" in file_output,
        f"cached TDLib library is not Mach-O arm64: {file_output.strip()}",
    )
    require(
        "x86_64" not in file_output,
        f"cached TDLib library is not arm64-only: {file_output.strip()}",
    )
    print(file_output.strip())
    return manifest


def restore(
    cache_root: Path,
    out_dir: Path,
    *,
    file_runner: FileRunner = default_file_runner,
) -> dict:
    key = cache_key()
    source = cache_root / key / "out"
    if not source.is_dir() or source.is_symlink():
        raise ColdCacheError(
            f"runner-local TDLib cache miss for key {key}; seed it from an arm64 "
            f"host with 'make tdlib', then copy .temp/tdlib/out to {source}"
        )
    out_dir.parent.mkdir(parents=True, exist_ok=True)
    scratch = out_dir.parent / f".{out_dir.name}.restore-{os.getpid()}"
    if scratch.exists():
        shutil.rmtree(scratch)
    try:
        shutil.copytree(source, scratch, symlinks=True)
        manifest = validate_artifact(scratch, file_runner=file_runner)
        if out_dir.exists() or out_dir.is_symlink():
            if out_dir.is_dir() and not out_dir.is_symlink():
                shutil.rmtree(out_dir)
            else:
                out_dir.unlink()
        scratch.rename(out_dir)
    finally:
        if scratch.exists():
            shutil.rmtree(scratch)
    print(
        "RESTORED PINNED TDLIB: "
        f"{key} sha256 {manifest['artifacts']['library']['sha256']}"
    )
    return manifest


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cache-root",
        type=Path,
        default=Path.home() / "gramdrive-ci" / "cache",
    )
    parser.add_argument("--out-dir", type=Path, default=Path(".temp/tdlib/out"))
    args = parser.parse_args(argv)
    try:
        restore(args.cache_root.resolve(), args.out_dir.resolve())
    except ColdCacheError as error:
        print(f"COLD CACHE: {error}", file=sys.stderr)
        return EXIT_COLD_CACHE
    except ArtifactError as error:
        print(f"INVALID CACHED TDLIB: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
