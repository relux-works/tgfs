#!/usr/bin/env python3
"""Shared-state smoke: two real processes over one shared container.

Owned by TASK-260715-gnsa2s. Proves the Apple shared durable state chain end
to end, in real separate processes, through the *packaged* artifact:

  1. A Rust seeder process (the coordinator/engine-host shape: FFI open,
     in-process writes) seeds a provider tree into a substitute container.
  2. Two Swift reader processes — running the GramDriveSupport package
     against the packaged GramDriveCore (SwiftPM -> XCFramework -> staticlib)
     — enumerate the same container concurrently; their outputs must be
     byte-identical and match what the seeder reported. That is the
     "two processes read consistent item metadata" acceptance proof.
  3. A Swift provider process runs the File Provider domain chain
     (TASK-260715-3s44pc): seeded accounts -> stable desired domain ->
     the real replicated-extension type resolving that domain back to the
     same account root the seeder reported.
  4. A Swift watcher process observes the change doorbell (Darwin
     notification, posted by a third Swift process) and the dataVersion
     change probe while the Rust seeder commits a mutation from yet another
     process; it must detect both and re-read the updated metadata.
  5. The two concurrent readers run again and must agree on the mutated
     state.

Requires: the Rust toolchain, Xcode (swift + xcodebuild), and a staged
artifact at .temp/packaging/GramDriveCore (`make package`; run automatically
when missing, or forced with --repackage).

Usage: python3 .scripts/smoke/run_shared_state_smoke.py [--repackage]
Artifacts and logs: .temp/shared-state-smoke/
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".temp" / "shared-state-smoke"
CORE_PACKAGE = REPO_ROOT / ".temp" / "packaging" / "GramDriveCore"
SUPPORT_PACKAGE = REPO_ROOT / "apple" / "GramDriveSupport"
CONTAINER = OUT_DIR / "container"
# Must match AppGroup.dataRootURL in the Swift package: the readers derive
# this from the container root on their own, and disagreement means empty
# reads and a failed smoke — that cross-check is deliberate.
DATA_ROOT = CONTAINER / "Library" / "Application Support" / "GramDrive"
WATCH_TIMEOUT = 30


def run(name: str, cmd: list[str], **kwargs) -> str:
    """Runs one step, teeing output to a log file; exits non-zero on failure."""
    log_path = OUT_DIR / f"{name}.log"
    print(f"--- {name}: {' '.join(str(c) for c in cmd)}")
    result = subprocess.run(
        cmd, cwd=REPO_ROOT, capture_output=True, text=True, **kwargs
    )
    log_path.write_text(result.stdout + result.stderr, encoding="utf-8")
    if result.returncode != 0:
        sys.stdout.write((result.stdout + result.stderr)[-4000:])
        print(f"FAILED: {name} (exit {result.returncode}); full log: {log_path}")
        sys.exit(result.returncode)
    return result.stdout


def parse_kv(text: str) -> dict[str, str]:
    facts = {}
    for line in text.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            facts[key.strip()] = value.strip()
    return facts


def seed(phase: str) -> dict[str, str]:
    stdout = run(
        f"seeder-{phase}",
        [
            "cargo", "run", "-q", "-p", "gramdrive-ffi",
            "--example", "shared_state_seed", "--", str(DATA_ROOT), phase,
        ],
    )
    facts = parse_kv(stdout)
    for key in ("root", "chat", "file", "file_content_version"):
        if key not in facts:
            print(f"FAILED: seeder-{phase} reported no '{key}'")
            sys.exit(1)
    return facts


def concurrent_reads(name: str, smoke_bin: Path, root_id: str) -> str:
    """Two reader processes at once; returns their (identical) output."""
    print(f"--- {name}: two concurrent reader processes")
    readers = [
        subprocess.Popen(
            [
                str(smoke_bin), "--container", str(CONTAINER),
                "--mode", "read", "--root", root_id,
            ],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        for _ in range(2)
    ]
    outputs = []
    for index, reader in enumerate(readers):
        stdout, stderr = reader.communicate(timeout=60)
        (OUT_DIR / f"{name}-reader-{index}.log").write_text(
            stdout + stderr, encoding="utf-8"
        )
        if reader.returncode != 0:
            sys.stdout.write(stdout + stderr)
            print(f"FAILED: {name} reader {index} (exit {reader.returncode})")
            sys.exit(1)
        outputs.append(stdout)
    if outputs[0] != outputs[1]:
        print(f"FAILED: {name}: the two processes disagree")
        for index, output in enumerate(outputs):
            print(f"--- reader {index} ---\n{output}")
        sys.exit(1)
    if "item id=" not in outputs[0]:
        print(f"FAILED: {name}: readers saw no items\n{outputs[0]}")
        sys.exit(1)
    return outputs[0]


def expect(name: str, output: str, needle: str) -> None:
    if needle not in output:
        print(f"FAILED: {name}: expected {needle!r} in:\n{output}")
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repackage", action="store_true",
        help="rebuild the packaged artifact even if one is staged",
    )
    args = parser.parse_args()

    if OUT_DIR.exists():
        shutil.rmtree(OUT_DIR)
    OUT_DIR.mkdir(parents=True)

    # 0. The packaged artifact the Swift side consumes.
    if args.repackage or not (CORE_PACKAGE / "Package.swift").exists():
        run("package", ["make", "package"])

    # 1. Build the support package once; concurrent `swift run` would race
    #    on the build directory, so the readers run the built binary.
    run(
        "swift-build",
        ["swift", "build", "--package-path", str(SUPPORT_PACKAGE)],
    )
    bin_path = run(
        "swift-bin-path",
        ["swift", "build", "--package-path", str(SUPPORT_PACKAGE), "--show-bin-path"],
    ).strip()
    smoke_bin = Path(bin_path) / "gramdrive-shared-state-smoke"

    # 2. Seed through the coordinator path, then read from two concurrent
    #    provider processes.
    facts = seed("seed")
    first = concurrent_reads("initial", smoke_bin, facts["root"])
    expect("initial", first, f"item id={facts['file']}")
    expect("initial", first, "content=c1")
    expect("initial", first, "size=2048")

    # 2b. File Provider domain chain (TASK-260715-3s44pc): a separate
    #     provider process maps the seeded account to its stable domain and
    #     the real extension type resolves that domain back to the same
    #     root item the Rust coordinator reported.
    domains = run(
        "domains",
        [
            str(smoke_bin), "--container", str(CONTAINER),
            "--mode", "domains",
        ],
    )
    expect("domains", domains, "accounts_count=1")
    expect("domains", domains, "account_id=7")
    expect("domains", domains, f"account_root={facts['root']}")
    expect("domains", domains, "domain_id=account-7")
    expect("domains", domains, "domain_name=GramDrive")
    expect("domains", domains, f"context_root={facts['root']}")

    # 3. Change flow: watcher (provider) must see the doorbell AND the
    #    moved data version around a foreign commit, then the new facts.
    print("--- watch: provider watcher across a foreign commit")
    watcher = subprocess.Popen(
        [
            str(smoke_bin), "--container", str(CONTAINER),
            "--mode", "watch", "--root", facts["file"],
            "--timeout", str(WATCH_TIMEOUT),
        ],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    assert watcher.stdout is not None
    ready = watcher.stdout.readline()
    if not ready.startswith("WATCH-READY"):
        watcher.kill()
        print(f"FAILED: watcher never became ready: {ready!r}")
        sys.exit(1)

    mutated = seed("mutate")
    ring = subprocess.run(
        [str(smoke_bin), "--mode", "signal"],
        capture_output=True, text=True, timeout=30,
    )
    if ring.returncode != 0 or "SIGNALED" not in ring.stdout:
        watcher.kill()
        print(f"FAILED: doorbell poster: {ring.stdout}{ring.stderr}")
        sys.exit(1)

    try:
        watch_output, _ = watcher.communicate(timeout=WATCH_TIMEOUT + 5)
    except subprocess.TimeoutExpired:
        watcher.kill()
        print("FAILED: watcher never observed the change")
        sys.exit(1)
    (OUT_DIR / "watch.log").write_text(ready + watch_output, encoding="utf-8")
    if watcher.returncode != 0:
        print(f"FAILED: watcher (exit {watcher.returncode}):\n{watch_output}")
        sys.exit(1)
    expect("watch", watch_output, "CHANGED signaled=true")
    expect("watch", watch_output, f"content={mutated['file_content_version']}")

    # 4. Two concurrent readers again, over the mutated state.
    second = concurrent_reads("mutated", smoke_bin, facts["root"])
    expect("mutated", second, f"item id={facts['file']}")
    expect("mutated", second, "content=c2")
    expect("mutated", second, "size=4096")

    print("SHARED-STATE SMOKE PASSED")
    print(f"  container: {CONTAINER}")
    print(f"  logs:      {OUT_DIR}")


if __name__ == "__main__":
    main()
