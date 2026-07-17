#!/usr/bin/env python3
"""Bindings smoke gate: prove the UniFFI contract from real native consumers.

Owned by TASK-260715-265gqq. Builds the FFI library, generates Swift and
Kotlin bindings with the workspace-local uniffi-bindgen, compiles the smoke
consumers in .scripts/smoke/{swift,kotlin}/ against them, and runs both.
The consumers assert the acceptance criteria: compilation against generated
bindings, async success with progress callbacks, structured error
round-trips, and CancellationToken cancellation round-trips.

Requires beyond the Rust toolchain (README.md § Tools):
  - swiftc      (Xcode command line tools; reference host per POL-5)
  - kotlinc     (brew install kotlin)
  - java 17+    (brew install openjdk)
Kotlin's runtime jars (JNA, kotlinx-coroutines) are downloaded once from
Maven Central, pinned by version and sha256 below, and cached in
.temp/bindings-smoke/jars/.

Usage: python3 .scripts/smoke/run_bindings_smoke.py [--skip-kotlin] [--skip-swift]
Artifacts and logs: .temp/bindings-smoke/
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".temp" / "bindings-smoke"
BINDINGS_DIR = OUT_DIR / "bindings"
JARS_DIR = OUT_DIR / "jars"
SMOKE_DIR = REPO_ROOT / ".scripts" / "smoke"
DYLIB_DEBUG_DIR = REPO_ROOT / "target" / "debug"

# Runtime jars for the Kotlin consumer, pinned by exact version and sha256.
# Bump deliberately; a hash mismatch fails the run rather than trusting the
# mirror (same spirit as the cargo-deny sources gate, POL-6/NFR-050).
MAVEN = "https://repo1.maven.org/maven2"
JARS = {
    f"{MAVEN}/net/java/dev/jna/jna/5.17.0/jna-5.17.0.jar": (
        "jna-5.17.0.jar",
        "b3a9408e7c51e08ef0e3bfcc08f443f6ec0f6191ba8cd7c18d53d2b22e5bdbc0",
    ),
    f"{MAVEN}/org/jetbrains/kotlinx/kotlinx-coroutines-core-jvm/1.10.2/"
    "kotlinx-coroutines-core-jvm-1.10.2.jar": (
        "kotlinx-coroutines-core-jvm-1.10.2.jar",
        "5ca175b38df331fd64155b35cd8cae1251fa9ee369709b36d42e0a288ccce3fd",
    ),
}


def run(name: str, cmd: list[str], **kwargs) -> None:
    """Runs one step, teeing output to a log file; exits non-zero on failure."""
    log_path = OUT_DIR / f"{name}.log"
    print(f"--- {name}: {' '.join(str(c) for c in cmd)}")
    with log_path.open("w", encoding="utf-8") as log:
        result = subprocess.run(
            cmd, cwd=REPO_ROOT, stdout=log, stderr=subprocess.STDOUT, **kwargs
        )
    if result.returncode != 0:
        sys.stdout.write(log_path.read_text(encoding="utf-8")[-4000:])
        print(f"FAILED: {name} (exit {result.returncode}); full log: {log_path}")
        sys.exit(result.returncode)
    tail = log_path.read_text(encoding="utf-8").strip().splitlines()[-3:]
    for line in tail:
        print(f"    {line}")


def ensure_jars() -> list[Path]:
    JARS_DIR.mkdir(parents=True, exist_ok=True)
    paths = []
    for url, (filename, expected_sha) in JARS.items():
        path = JARS_DIR / filename
        if not path.exists():
            print(f"--- fetch {filename}")
            with urllib.request.urlopen(url) as response:
                path.write_bytes(response.read())
        actual_sha = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual_sha != expected_sha:
            print(f"FAILED: sha256 mismatch for {filename}")
            print(f"  expected {expected_sha}")
            print(f"  actual   {actual_sha}")
            sys.exit(1)
        paths.append(path)
    return paths


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-swift", action="store_true")
    parser.add_argument("--skip-kotlin", action="store_true")
    args = parser.parse_args()

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    if BINDINGS_DIR.exists():
        shutil.rmtree(BINDINGS_DIR)

    run("cargo-build", ["cargo", "build", "-p", "gramdrive-ffi"])
    run(
        "uniffi-bindgen",
        [
            "cargo", "run", "-p", "gramdrive-ffi", "--features", "bindgen",
            "--bin", "uniffi-bindgen", "--", "generate",
            "--library", str(DYLIB_DEBUG_DIR / "libgramdrive_ffi.dylib"),
            "--language", "swift", "--language", "kotlin",
            "--out-dir", str(BINDINGS_DIR),
        ],
    )

    if not args.skip_swift:
        # The staticlib is linked directly: no install-name/dyld games, and
        # it proves the artifact Apple hosts will actually embed.
        run(
            "swift-compile",
            [
                "xcrun", "swiftc",
                "-o", str(OUT_DIR / "smoke-swift"),
                str(SMOKE_DIR / "swift" / "main.swift"),
                str(BINDINGS_DIR / "GramDriveCore.swift"),
                "-I", str(BINDINGS_DIR),
                "-Xcc", f"-fmodule-map-file={BINDINGS_DIR / 'GramDriveCoreFFI.modulemap'}",
                str(DYLIB_DEBUG_DIR / "libgramdrive_ffi.a"),
            ],
        )
        run("swift-run", [str(OUT_DIR / "smoke-swift")])

    if not args.skip_kotlin:
        jars = ensure_jars()
        classpath = ":".join(str(j) for j in jars)
        kotlin_bindings = (
            BINDINGS_DIR / "com" / "reluxworks" / "gramdrive" / "core" / "gramdrive.kt"
        )
        smoke_jar = OUT_DIR / "smoke-kotlin.jar"
        run(
            "kotlin-compile",
            [
                "kotlinc",
                str(kotlin_bindings),
                str(SMOKE_DIR / "kotlin" / "Main.kt"),
                "-classpath", classpath,
                "-include-runtime",
                "-d", str(smoke_jar),
            ],
        )
        # The JVM consumer loads the cdylib via JNA, which resolves
        # libgramdrive_ffi.dylib through jna.library.path.
        run(
            "kotlin-run",
            [
                "java",
                "-cp", f"{smoke_jar}:{classpath}",
                f"-Djna.library.path={DYLIB_DEBUG_DIR}",
                "MainKt",
            ],
        )

    print("BINDINGS SMOKE PASSED")


if __name__ == "__main__":
    main()
