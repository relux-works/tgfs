#!/usr/bin/env python3
"""Negative-check harness for .scripts/check_crate_architecture.py.

Copies the workspace to a scratch tree, injects one violation per case, runs
the check, and prints the errors + exit code. The real tree is never touched.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRATCH = Path(__file__).resolve().parent / "scratch"

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


def run_check(root: Path) -> tuple[str, int]:
    proc = subprocess.run(
        [sys.executable, ".scripts/check_crate_architecture.py"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    return (proc.stdout + proc.stderr).strip(), proc.returncode


def add_dep(root: Path, crate: str, line: str) -> None:
    """Append a line under the crate's existing [dependencies] table."""
    path = root / f"crates/{crate}/Cargo.toml"
    text = path.read_text(encoding="utf-8")
    marker = "[dependencies]\n"
    index = text.index(marker) + len(marker)
    path.write_text(text[:index] + line + "\n" + text[index:], encoding="utf-8")


# Each case: (title, mutate(root) -> None)
CASES: list[tuple[str, object]] = [
    (
        "NEG-1: direction violation + dev-only crate as a normal dep",
        lambda r: (
            add_dep(
                r, "gramdrive-render", 'gramdrive-state = { path = "../gramdrive-state" }'
            ),
            add_dep(
                r,
                "gramdrive-state",
                'gramdrive-testkit = { path = "../gramdrive-testkit" }',
            ),
        ),
    ),
    (
        "NEG-3: crate README missing",
        lambda r: (r / "crates/gramdrive-render/README.md").unlink(),
    ),
    (
        "D1a: cfg(all(unix, ...)) nested predicate",
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg(all(unix, feature = "never"))]\npub fn probe_all() {}\n',
        ),
    ),
    (
        "D1b: cfg(not(windows)) negated predicate",
        lambda r: append_src(
            r, MODEL_SRC, "\n#[cfg(not(windows))]\npub fn probe_not() {}\n"
        ),
    ),
    (
        "D1c: cfg_attr(windows, ...) attribute form",
        lambda r: append_src(
            r,
            MODEL_SRC,
            "\n#[cfg_attr(windows, allow(dead_code))]\npub fn probe_attr() {}\n",
        ),
    ),
    (
        "D1d: cfg!(target_os = ..) macro form",
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\npub fn probe_macro() -> bool {\n    cfg!(target_os = "macos")\n}\n',
        ),
    ),
    (
        "D1e: cfg(...) predicate wrapped across lines",
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg(all(\n    target_family = "wasm",\n    feature = "never"\n))]\npub fn probe_wrapped() {}\n',
        ),
    ),
    (
        "D1f: simple cfg(target_os = ..) (NEG-2 regression guard)",
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg(target_os = "linux")]\npub fn probe_simple() {}\n',
        ),
    ),
    (
        "D2: target-gated dependency in a platform-neutral crate",
        lambda r: (r / STATE_TOML).write_text(
            (r / STATE_TOML).read_text(encoding="utf-8")
            + '\n[target.\'cfg(target_os = "macos")\'.dependencies]\n'
            'gramdrive-model = { path = "../gramdrive-model" }\n',
            encoding="utf-8",
        ),
    ),
    (
        "Optional: std::os:: path with no cfg and no dependency",
        lambda r: append_src(
            r,
            MODEL_SRC,
            "\nuse std::os::unix::ffi::OsStrExt;\n",
        ),
    ),
    (
        "Control: doc comments naming cfg(target_os/windows/unix) stay clean",
        lambda r: append_src(
            r,
            MODEL_SRC,
            "\n//! - no platform-specific `cfg(target_os/windows/unix)` code\n"
            "/// Mentions cfg(unix) and cfg!(target_os = \"macos\") in prose.\n"
            "pub fn probe_docs() {}\n",
        ),
    ),
    (
        "Control: cfg(test)/cfg(feature) stay clean",
        lambda r: append_src(
            r,
            MODEL_SRC,
            '\n#[cfg(test)]\nmod probe_gate {}\n'
            '\n#[cfg(feature = "never")]\npub fn probe_feature() {}\n',
        ),
    ),
]


def main() -> int:
    for title, mutate in CASES:
        root = fresh_copy()
        mutate(root)
        output, code = run_check(root)
        print(f"=== {title} ===")
        print(output)
        print(f"exit={code}\n")
    shutil.rmtree(SCRATCH)
    return 0


if __name__ == "__main__":
    sys.exit(main())
