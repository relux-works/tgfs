#!/usr/bin/env python3
"""Reviewer's independent adversarial probes for check_crate_architecture.py.

Forms deliberately NOT present in the implementer's harness. Copies the
workspace to a scratch tree per case; the real tree is never touched.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRATCH = Path(__file__).resolve().parent / "scratch2"

MODEL_SRC = "crates/gramdrive-model/src/lib.rs"
STATE_TOML = "crates/gramdrive-state/Cargo.toml"


def fresh_copy() -> Path:
    if SCRATCH.exists():
        shutil.rmtree(SCRATCH)
    SCRATCH.mkdir(parents=True)
    for item in ("crates", "Cargo.toml", "Cargo.lock", ".scripts"):
        src = REPO / item
        dst = SCRATCH / item
        if src.is_dir():
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)
    return SCRATCH


def append_src(root: Path, rel: str, snippet: str) -> None:
    path = root / rel
    path.write_text(path.read_text(encoding="utf-8") + snippet, encoding="utf-8")


def append_toml(root: Path, rel: str, snippet: str) -> None:
    path = root / rel
    path.write_text(path.read_text(encoding="utf-8") + snippet, encoding="utf-8")


def run_check(root: Path) -> tuple[str, int]:
    proc = subprocess.run(
        [sys.executable, ".scripts/check_crate_architecture.py"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    return (proc.stdout + proc.stderr).strip(), proc.returncode


# (title, expected_exit, mutate)
CASES = [
    (
        "R1: cfg(any(unix, ..)) — original any() form, regression",
        1,
        lambda r: append_src(
            r, MODEL_SRC, '\n#[cfg(any(unix, feature = "never"))]\npub fn p1() {}\n'
        ),
    ),
    (
        "R2: bare cfg!(windows) macro predicate",
        1,
        lambda r: append_src(
            r, MODEL_SRC, "\npub fn p2() -> bool {\n    cfg!(windows)\n}\n"
        ),
    ),
    (
        "R3: target-gated DEV-dependency",
        1,
        lambda r: append_toml(
            r,
            STATE_TOML,
            "\n[target.'cfg(windows)'.dev-dependencies]\n"
            'gramdrive-model = { path = "../gramdrive-model" }\n',
        ),
    ),
    (
        "R4: target-gated BUILD-dependency",
        1,
        lambda r: append_toml(
            r,
            STATE_TOML,
            "\n[target.'cfg(unix)'.build-dependencies]\n"
            'gramdrive-model = { path = "../gramdrive-model" }\n',
        ),
    ),
    (
        "R5: plain-triple target gate (no cfg syntax at all)",
        1,
        lambda r: append_toml(
            r,
            STATE_TOML,
            "\n[target.x86_64-pc-windows-msvc.dependencies]\n"
            'gramdrive-model = { path = "../gramdrive-model" }\n',
        ),
    ),
    (
        "R6: banned platform dep in DEV section (check 6, dev incl.)",
        1,
        lambda r: append_toml(
            r,
            STATE_TOML,
            '\n[dev-dependencies]\nwinapi = "0.3"\n',
        ),
    ),
    (
        "R7: renamed target-gated dep (reviewer's original probe C form)",
        1,
        lambda r: append_toml(
            r,
            STATE_TOML,
            "\n[target.'cfg(target_os = \"macos\")'.dependencies]\n"
            'gramdrive-model2 = { path = "../gramdrive-model", package = "gramdrive-model" }\n',
        ),
    ),
    (
        "R8: multi-line cfg_attr, predicate on continuation line",
        1,
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg_attr(\n    target_vendor = "apple",\n    allow(dead_code)\n)]\npub fn p8() {}\n',
        ),
    ),
    (
        "R9-obs: target_arch/target_env predicates (documented out of scope)",
        0,
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg(target_arch = "wasm32")]\npub fn p9a() {}\n'
            '#[cfg(target_env = "msvc")]\npub fn p9b() {}\n',
        ),
    ),
    (
        "R10-control: predicate word in plain string, no cfg — stays clean",
        0,
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\npub fn p10() -> &\'static str {\n    "runs on windows and unix"\n}\n',
        ),
    ),
]


def main() -> int:
    failures = 0
    for title, expected, mutate in CASES:
        root = fresh_copy()
        mutate(root)
        output, code = run_check(root)
        verdict = "PASS" if code == expected else "MISMATCH"
        if verdict != "PASS":
            failures += 1
        print(f"=== {title} ===")
        print(output)
        print(f"exit={code} expected={expected} -> {verdict}\n")
    shutil.rmtree(SCRATCH)
    print(f"probe summary: {len(CASES) - failures}/{len(CASES)} as expected")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
