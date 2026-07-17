#!/usr/bin/env python3
"""Enforce the GramDrive Rust workspace architecture (crates/README.md).

Checks, all fatal (exit code 1):
  1. The workspace member set matches the policy table exactly — a new crate
     fails the check until crates/README.md and POLICY below both list it.
  2. Internal [dependencies]/[build-dependencies] of each crate are a subset
     of its allowed set (dependency direction, DEC-003 layering).
  3. gramdrive-testkit never appears in [dependencies]/[build-dependencies]
     of another workspace crate (dev-dependencies are fine).
  4. Nothing depends on gramdrive-ffi (top of the graph).
  5. The actual internal dependency graph is acyclic (checked independently
     of the allow list).
  6. No crate marked platform-neutral has a direct dependency from the
     platform ban list, in any dependency section including dev.
  7. No crate marked platform-neutral has a target-gated dependency
     ([target.'cfg(...)'.dependencies]), in any section including dev — a
     platform-conditional dep is leakage whatever the dep is named.
  8. No platform cfg predicates (target_os/target_family/target_vendor/
     windows/unix) appear in platform-neutral crate sources, in any of the
     cfg(...), cfg!(...) and cfg_attr(...) forms, including nested all()/
     not()/any() and arguments wrapped across lines.
  9. No std::os:: paths in platform-neutral crate sources — those compile
     per-platform with no cfg and no dependency to give them away.
 10. Every crate directory has a README.md with '## Ownership' and
     '## Test command' sections.
 11. Every crate opts into the shared lint set with `[lints] workspace = true`.
     A crate that omits it still compiles and still passes the lint gate — it
     is simply exempt from every lint in [workspace.lints], silently. That is
     the failure mode this check exists for: a green gate that checked nothing.

Scan tradeoffs for checks 8 and 9 — deliberately fail-closed, since a false
positive costs one rename and a miss costs the platform-neutrality guarantee:
  - Only `//` line comments are stripped. Predicate words inside block
    comments or string literals are flagged; keep prose out of /* */.
  - Stripping treats `//` inside a string literal as a comment start, so a
    line whose predicate follows a literal `//` (e.g. a URL) is missed.
  - A cfg invocation with unbalanced parentheses is scanned to end-of-file.

Requires: cargo on PATH (uses `cargo metadata`).
Usage: python3 .scripts/check_crate_architecture.py [--repo-root PATH]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# ---------------------------------------------------------------------------
# Policy table — the executable mirror of crates/README.md. Keep both in sync.
# ---------------------------------------------------------------------------

CORE_CRATES = {
    "gramdrive-model",
    "gramdrive-source",
    "gramdrive-source-tdjson",
    "gramdrive-state",
    "gramdrive-render",
    "gramdrive-engine",
    "gramdrive-ffi",
    "gramdrive-testkit",
}

# crate -> internal crates it may list in [dependencies]/[build-dependencies]
ALLOWED_INTERNAL_DEPS = {
    "gramdrive-model": set(),
    "gramdrive-source": {"gramdrive-model"},
    "gramdrive-source-tdjson": {"gramdrive-model", "gramdrive-source"},
    "gramdrive-state": {"gramdrive-model"},
    "gramdrive-render": {"gramdrive-model"},
    "gramdrive-engine": {
        "gramdrive-model",
        "gramdrive-source",
        "gramdrive-state",
        "gramdrive-render",
    },
    "gramdrive-ffi": {
        "gramdrive-model",
        "gramdrive-source",
        "gramdrive-source-tdjson",
        "gramdrive-state",
        "gramdrive-render",
        "gramdrive-engine",
    },
    "gramdrive-testkit": {
        "gramdrive-model",
        "gramdrive-source",
        "gramdrive-render",
    },
}

# Never a runtime/build dependency of any other workspace crate.
DEV_ONLY_CRATES = {"gramdrive-testkit"}

# Nothing inside the workspace may depend on these.
TOP_CRATES = {"gramdrive-ffi"}

# Crates that must stay free of platform-specific code. Future platform host
# crates (Windows CfAPI / Linux FUSE) join the workspace outside this set.
PLATFORM_NEUTRAL_CRATES = set(CORE_CRATES)

# Direct dependencies that indicate platform leakage in a neutral crate.
PLATFORM_BANNED_DEPS = {
    "windows",
    "windows-sys",
    "windows-core",
    "winapi",
    "fuser",
    "fuse3",
    "jni",
    "ndk",
    "ndk-sys",
    "android_logger",
    "objc",
    "objc2",
    "core-foundation",
    "core-foundation-sys",
    "security-framework",
    "swift-bridge",
}

# A cfg/cfg_attr/cfg! invocation, up to and including its opening delimiter.
CFG_INVOCATION_RE = re.compile(r"\bcfg(?:_attr)?\s*[!(]")

# A platform predicate anywhere inside a cfg argument list.
PLATFORM_PREDICATE_RE = re.compile(
    r"\b(?:target_os|target_family|target_vendor|windows|unix)\b"
)

STD_OS_RE = re.compile(r"\bstd\s*::\s*os\s*::")

REQUIRED_README_SECTIONS = ("## Ownership", "## Test command")


def load_workspace(repo_root: Path) -> list[dict]:
    proc = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=repo_root,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        sys.exit(f"error: cargo metadata failed with exit code {proc.returncode}")
    return json.loads(proc.stdout)["packages"]


def strip_line_comments(text: str) -> str:
    """Drop `//` comments while preserving line numbering."""
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def platform_cfg_lines(code: str) -> list[int]:
    """1-based line numbers of cfg invocations naming a platform predicate.

    Scans the balanced-parenthesis argument span of every cfg/cfg_attr/cfg!
    invocation, so nested all()/not()/any() and arguments wrapped across
    lines are covered.
    """
    hits: list[int] = []
    for match in CFG_INVOCATION_RE.finditer(code):
        start = code.find("(", match.end() - 1)
        if start == -1:
            continue
        depth = 0
        end = len(code)  # unbalanced: scan to EOF rather than miss the predicate
        for index in range(start, len(code)):
            if code[index] == "(":
                depth += 1
            elif code[index] == ")":
                depth -= 1
                if depth == 0:
                    end = index + 1
                    break
        if PLATFORM_PREDICATE_RE.search(code[start:end]):
            hits.append(code.count("\n", 0, match.start()) + 1)
    return hits


def std_os_lines(code: str) -> list[int]:
    return [
        code.count("\n", 0, match.start()) + 1 for match in STD_OS_RE.finditer(code)
    ]


def check(repo_root: Path) -> list[str]:
    errors: list[str] = []
    packages = load_workspace(repo_root)
    members = {pkg["name"]: pkg for pkg in packages}

    # 1. Member set matches the policy table.
    unknown = sorted(set(members) - CORE_CRATES)
    missing = sorted(CORE_CRATES - set(members))
    for name in unknown:
        errors.append(
            f"{name}: workspace member has no policy row — add it to "
            f"crates/README.md and POLICY tables in this script"
        )
    for name in missing:
        errors.append(f"{name}: listed in policy but missing from the workspace")

    # Dependency edges by kind. cargo metadata: kind null=normal, "dev", "build".
    # `target` is null unless the dep sits under [target.'cfg(...)'.dependencies].
    normal_edges: dict[str, set[str]] = {}
    all_direct: dict[str, list[tuple[str, str, str | None]]] = {}
    for name, pkg in members.items():
        normal_edges[name] = set()
        direct: set[tuple[str, str, str | None]] = set()
        for dep in pkg["dependencies"]:
            kind = dep["kind"] or "normal"
            direct.add((dep["name"], kind, dep.get("target")))
            if dep["name"] in members and kind in ("normal", "build"):
                normal_edges[name].add(dep["name"])
        all_direct[name] = sorted(direct, key=lambda d: (d[0], d[1], d[2] or ""))

    for name in sorted(set(members) & CORE_CRATES):
        # 2. Direction allow list.
        allowed = ALLOWED_INTERNAL_DEPS[name]
        for dep in sorted(normal_edges[name] - allowed):
            errors.append(
                f"{name}: internal dependency '{dep}' violates the documented "
                f"direction (allowed: {sorted(allowed) or 'none'})"
            )
        # 3. Dev-only crates never ship.
        for dep in sorted(normal_edges[name] & DEV_ONLY_CRATES):
            errors.append(
                f"{name}: '{dep}' is test support and may only be a dev-dependency"
            )
        # 4. Top-of-graph crates.
        for dep in sorted(normal_edges[name] & TOP_CRATES):
            errors.append(f"{name}: nothing may depend on '{dep}'")
        if name in PLATFORM_NEUTRAL_CRATES:
            for dep, kind, target in all_direct[name]:
                # 6. Platform-banned direct deps (any section, dev included).
                if dep in PLATFORM_BANNED_DEPS:
                    errors.append(
                        f"{name}: platform-specific dependency '{dep}' ({kind}) "
                        f"is forbidden in a platform-neutral crate"
                    )
                # 7. Target-gated deps (any section, dev included).
                if target is not None:
                    errors.append(
                        f"{name}: dependency '{dep}' ({kind}) is target-gated on "
                        f"[target.'{target}'.dependencies] — platform-conditional "
                        f"dependencies are forbidden in a platform-neutral crate"
                    )

    # 5. Cycle check on the actual internal graph (Kahn's algorithm).
    graph = {name: set(deps) for name, deps in normal_edges.items()}
    while True:
        leaves = [n for n, deps in graph.items() if not deps]
        if not leaves:
            break
        for leaf in leaves:
            graph.pop(leaf)
            for deps in graph.values():
                deps.discard(leaf)
    if graph:
        errors.append(f"dependency cycle among: {sorted(graph)}")

    # 8/9. Platform predicates in sources; 10. README sections; 11. lints opt-in.
    for name in sorted(set(members) & CORE_CRATES):
        manifest_path = Path(members[name]["manifest_path"])
        crate_dir = manifest_path.parent

        # 11. Shared lint set opt-in. cargo metadata does not report the
        # [lints] table, so read the manifest directly.
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        if manifest.get("lints", {}).get("workspace") is not True:
            errors.append(
                f"{name}: Cargo.toml lacks '[lints] workspace = true' — the "
                f"crate is silently exempt from the shared lint set in "
                f"[workspace.lints]"
            )

        if name in PLATFORM_NEUTRAL_CRATES:
            for src in sorted((crate_dir / "src").rglob("*.rs")):
                code = strip_line_comments(src.read_text(encoding="utf-8"))
                rel = src.relative_to(repo_root)
                for lineno in platform_cfg_lines(code):
                    errors.append(
                        f"{name}: platform cfg predicate at {rel}:{lineno}"
                    )
                for lineno in std_os_lines(code):
                    errors.append(
                        f"{name}: platform-specific 'std::os::' path at "
                        f"{rel}:{lineno}"
                    )
        readme = crate_dir / "README.md"
        if not readme.is_file():
            errors.append(f"{name}: missing README.md")
            continue
        text = readme.read_text(encoding="utf-8")
        for section in REQUIRED_README_SECTIONS:
            if section not in text:
                errors.append(f"{name}: README.md lacks a '{section}' section")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args()

    errors = check(args.repo_root.resolve())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        print(f"\ncrate architecture check FAILED: {len(errors)} error(s)", file=sys.stderr)
        return 1
    print(f"crate architecture check OK: {len(CORE_CRATES)} crates conform")
    return 0


if __name__ == "__main__":
    sys.exit(main())
