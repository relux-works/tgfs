#!/usr/bin/env python3
"""Run a GramDrive automated gate suite and record its provenance.

This is the single entrypoint for every automated check in the repo. Local runs
and CI jobs invoke the same script with the same suite names, so "it passes on
my machine" and "it passes in CI" are the same sentence. CI must not assemble
its own list of cargo commands: a gate that exists only inside a YAML file
cannot be run before pushing, and drifts from the local one the first time
someone edits either.

    python3 .scripts/acceptance/run_automated.py --suite core --run-id local-core
    python3 .scripts/acceptance/run_automated.py --suite all --run-id ci-all --require-clean
    python3 .scripts/acceptance/run_automated.py --list

Every step runs even after an earlier one fails, because the useful output of a
gate is the full list of what is broken, not the first thing it tripped over.

Provenance for each run is written to .temp/acceptance/<run-id>/ (gitignored):

    summary.json    machine-readable result: commit, worktree state, tool
                    versions, and every step's command, exit code and duration
    <step>.log      combined stdout+stderr of that step

CI uploads that directory as an artifact (NFR-052: a result has to be
attributable to a commit). `--require-clean` refuses to run against a dirty
worktree, which is what makes the recorded commit sha mean something.

Exit codes:
    0   every step passed
    1   at least one step failed
    2   the run could not start (bad arguments, dirty worktree)

Requires: cargo, rustc, git and cargo-deny on PATH (`--suite core` verifies
this in its first step); gitleaks on PATH for `--suite security`. Python 3.11+,
stdlib only.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import time
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

PROVENANCE_ROOT = Path(".temp") / "acceptance"

# A run id names a directory under .temp/acceptance/ and appears in artifact
# names. Anchored, no separators: "../../etc" must never become a write path.
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_CANNOT_START = 2


@dataclass(frozen=True)
class Step:
    """One gate command and why it exists."""

    name: str
    argv: tuple[str, ...]
    purpose: str


@dataclass
class StepResult:
    step: Step
    exit_code: int
    duration_seconds: float
    log_name: str

    @property
    def passed(self) -> bool:
        return self.exit_code == 0


def _python(script: str, *args: str) -> tuple[str, ...]:
    """Invoke a repo script with the interpreter running this one.

    Not "python3": on a machine with several interpreters, the one that found
    this script is the one whose tomllib version the gate was verified against.
    """
    return (sys.executable, script, *args)


def build_steps() -> dict[str, Step]:
    """The full catalog of gate steps, keyed by name."""
    return {
        step.name: step
        for step in (
            Step(
                name="toolchain",
                argv=_python(".scripts/check_toolchain.py"),
                purpose="active toolchain matches rust-toolchain.toml",
            ),
            Step(
                name="format",
                argv=("cargo", "fmt", "--all", "--check"),
                purpose="rustfmt.toml formatting",
            ),
            # --all-targets so tests and benches are linted too: a lint that
            # stops at the library misses most of the code people actually
            # write. --all-features matches deny.toml's graph, so the lint set
            # and the license set describe the same build.
            Step(
                name="lint",
                argv=(
                    "cargo",
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-D",
                    "warnings",
                ),
                purpose="clippy lint set from [workspace.lints], warnings are errors",
            ),
            Step(
                name="test",
                argv=("cargo", "test", "--workspace", "--all-features"),
                purpose="unit and integration tests across the workspace",
            ),
            Step(
                name="architecture",
                argv=_python(".scripts/check_crate_architecture.py"),
                purpose="crate layering, dependency direction, platform neutrality",
            ),
            Step(
                name="supply-chain",
                argv=("cargo", "deny", "check"),
                purpose="POL-6 licenses, RustSec advisories, bans, sources",
            ),
            Step(
                name="traceability",
                argv=_python(".scripts/validate_traceability.py"),
                purpose="docs/TRACEABILITY.md against .spec/ and .task-board/",
            ),
            # The gate scripts are the thing every other gate is trusted
            # through; an untested runner is a gate with no gate.
            Step(
                name="scripts",
                # Top-level dir is the tests dir itself: unittest requires an
                # importable start directory, and `.scripts` is not a legal
                # package name.
                argv=_python(
                    "-m",
                    "unittest",
                    "discover",
                    "-s",
                    ".scripts/tests",
                    "-t",
                    ".scripts/tests",
                ),
                purpose="self-tests for the scripts in .scripts/",
            ),
            # The macOS native leg (POL-5 / DEC-017 reference target). Both
            # steps compile and test apple/GramDriveSupport against the staged
            # GramDriveCore artifact, so they need macOS + Xcode and a prior
            # `make package` (like the smokes). That is why they are their own
            # `apple` suite, never folded into `all`, which must run on any host
            # without Xcode or the staged core. native-ci stages the core first,
            # then runs this suite through the same entrypoint local devs use.
            Step(
                name="swift-build",
                argv=("swift", "build", "--package-path", "apple/GramDriveSupport"),
                purpose="apple/GramDriveSupport compiles against the staged GramDriveCore (needs `make package` first)",
            ),
            Step(
                name="swift-test",
                argv=("swift", "test", "--package-path", "apple/GramDriveSupport"),
                purpose="apple/GramDriveSupport unit tests: File Provider, agent, companion, shared state",
            ),
            # git mode, not the working tree: a secret that was committed and
            # later deleted still ships in every clone's history. --redact keeps
            # the matched value out of the provenance log (the AC is "logs
            # contain no secrets"); .gitleaks.toml + .gitleaksignore are the
            # committed, pinned config so local and CI verdicts agree.
            Step(
                name="secret-scan",
                argv=(
                    "gitleaks",
                    "git",
                    ".",
                    "--config",
                    ".gitleaks.toml",
                    "--redact",
                    "--no-banner",
                ),
                purpose="gitleaks: no secret in committed history (redacted output)",
            ),
        )
    }


# Suite -> the steps it runs, in order. Suites match CI job boundaries: one job
# per suite, per the barycenter one-job-per-component pattern.
SUITES: dict[str, tuple[str, ...]] = {
    # Everything that guards the Rust core. This is the pre-push gate.
    "core": (
        "toolchain",
        "format",
        "lint",
        "test",
        "architecture",
        "supply-chain",
    ),
    # Repo-level documentation and tooling gates. No Rust toolchain needed,
    # so CI can run this on any runner.
    "repo": (
        "traceability",
        "scripts",
    ),
    # The macOS native leg. Its own suite (not in `all`) because it needs macOS
    # + Xcode and the staged core; native-ci runs it as its own job after
    # staging `make package`. Build then test, same order as the core suite.
    "apple": (
        "swift-build",
        "swift-test",
    ),
    # Secret scanning. Its own suite, deliberately NOT folded into `all`: it is
    # the only gate that needs gitleaks on PATH (a third tool footprint after
    # core's Rust and repo's Python) and it is a merge-boundary check, so the
    # everyday pre-push `all` run stays gitleaks-free while CI runs this as its
    # own required job.
    "security": ("secret-scan",),
    "all": ("core", "repo"),
}


def resolve_suite(suite: str, catalog: dict[str, Step]) -> list[Step]:
    """Expand a suite name into its steps, flattening nested suites.

    A step name is also accepted and resolves to a one-step run. That keeps
    "just re-run the license check" reachable through this entrypoint instead
    of sending people back to a hand-typed cargo command that no gate agrees
    with.

    Raises KeyError if the name is neither a suite nor a step.
    """
    if suite in catalog and suite not in SUITES:
        return [catalog[suite]]
    if suite not in SUITES:
        raise KeyError(suite)

    ordered: list[str] = []
    pending = list(SUITES[suite])
    while pending:
        name = pending.pop(0)
        if name in SUITES:
            pending = list(SUITES[name]) + pending
        elif name not in ordered:
            ordered.append(name)
    return [catalog[name] for name in ordered]


def default_runner(argv: Sequence[str], cwd: Path) -> tuple[int, str]:
    """Run argv in cwd, returning (exit_code, combined output)."""
    try:
        proc = subprocess.run(
            list(argv),
            cwd=cwd,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        # A missing tool is a failed gate with a readable reason, not a
        # traceback out of the runner itself.
        return 127, f"{argv[0]}: not found on PATH\n"
    return proc.returncode, proc.stdout + proc.stderr


Runner = Callable[[Sequence[str], Path], tuple[int, str]]


def git_state(repo_root: Path, runner: Runner) -> tuple[str | None, bool | None]:
    """Return (commit sha, worktree_clean). Either is None if git cannot say."""
    code, output = runner(("git", "rev-parse", "HEAD"), repo_root)
    commit = output.strip() if code == 0 else None

    code, output = runner(("git", "status", "--porcelain"), repo_root)
    clean = (output.strip() == "") if code == 0 else None
    return commit, clean


def tool_versions(repo_root: Path, runner: Runner) -> dict[str, str]:
    """First line of each gate tool's --version, for the provenance record.

    Recorded rather than asserted: `--suite core` asserts the toolchain pin in
    its own step, while this is the evidence a future reader needs to explain
    why a run behaved the way it did.
    """
    versions: dict[str, str] = {"python": sys.version.split()[0]}
    probes = {
        "rustc": ("rustc", "--version"),
        "cargo": ("cargo", "--version"),
        "cargo-deny": ("cargo", "deny", "--version"),
        "git": ("git", "--version"),
    }
    for name, argv in probes.items():
        code, output = runner(argv, repo_root)
        first_line = output.strip().splitlines()[0] if output.strip() else ""
        versions[name] = first_line if code == 0 else "unavailable"
    return versions


def provenance_dir(repo_root: Path, run_id: str) -> Path:
    return repo_root / PROVENANCE_ROOT / run_id


def run_suite(
    steps: Iterable[Step],
    *,
    repo_root: Path,
    run_id: str,
    suite: str,
    require_clean: bool,
    runner: Runner = default_runner,
    echo: Callable[[str], None] = print,
) -> tuple[dict, int]:
    """Run every step, write provenance, and return (summary, exit code).

    Steps are never skipped because an earlier one failed: the point of a gate
    run is to learn everything that is broken in one pass.
    """
    steps = list(steps)
    out_dir = provenance_dir(repo_root, run_id)
    # A stale log from a previous run under the same id is worse than no log,
    # so the directory is rebuilt rather than merged into.
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    commit, clean = git_state(repo_root, runner)
    started = datetime.now(UTC)

    summary: dict = {
        "schema": 1,
        "run_id": run_id,
        "suite": suite,
        "commit": commit,
        "worktree_clean": clean,
        "require_clean": require_clean,
        "tools": tool_versions(repo_root, runner),
        "started_at": started.isoformat(),
        "steps": [],
    }

    def finish(result: str, exit_code: int) -> tuple[dict, int]:
        summary["result"] = result
        summary["finished_at"] = datetime.now(UTC).isoformat()
        (out_dir / "summary.json").write_text(
            json.dumps(summary, indent=2) + "\n", encoding="utf-8"
        )
        return summary, exit_code

    # Refuse before running anything: a suite result stamped with a commit sha
    # that does not describe the tested tree is a false provenance record, and
    # a wrong record is worse than an absent one (NFR-052).
    if require_clean and clean is False:
        message = (
            "worktree has uncommitted changes and --require-clean was given; "
            "the recorded commit would not describe what was tested"
        )
        echo(f"ERROR: {message}")
        summary["error"] = message
        return finish("cannot-start", EXIT_CANNOT_START)

    results: list[StepResult] = []
    for step in steps:
        echo(f"==> {step.name}: {' '.join(step.argv)}")
        began = time.monotonic()
        code, output = runner(step.argv, repo_root)
        duration = time.monotonic() - began

        log_name = f"{step.name}.log"
        (out_dir / log_name).write_text(output, encoding="utf-8")

        result = StepResult(step, code, duration, log_name)
        results.append(result)
        summary["steps"].append(
            {
                "name": step.name,
                "purpose": step.purpose,
                "command": list(step.argv),
                "exit_code": code,
                "duration_seconds": round(duration, 3),
                "log": log_name,
                "status": "passed" if result.passed else "failed",
            }
        )
        if not result.passed:
            # Echo the failing step's output now; a developer should not have
            # to go find a log file to see why their gate failed.
            echo(output.rstrip())
        echo(f"<== {step.name}: {'ok' if result.passed else 'FAILED'} ({duration:.1f}s)")

    failed = [result for result in results if not result.passed]

    echo("")
    echo(f"suite '{suite}' ({run_id}): {len(results) - len(failed)}/{len(results)} passed")
    for result in results:
        mark = "ok  " if result.passed else "FAIL"
        echo(f"  [{mark}] {result.step.name} ({result.duration_seconds:.1f}s)")
    echo(f"provenance: {provenance_dir(Path('.'), run_id)}")

    if failed:
        echo("")
        echo("failed steps:")
        for result in failed:
            echo(
                f"  {result.step.name} (exit {result.exit_code}) "
                f"-> {(out_dir / result.log_name)}"
            )
        return finish("failed", EXIT_FAILED)
    return finish("passed", EXIT_OK)


def format_catalog() -> str:
    catalog = build_steps()
    lines = ["Suites:"]
    for suite in SUITES:
        step_names = [step.name for step in resolve_suite(suite, catalog)]
        lines.append(f"  {suite:<8} {', '.join(step_names)}")
    lines.append("")
    lines.append("Steps (any step name is also usable as --suite):")
    for step in catalog.values():
        lines.append(f"  {step.name:<14} {step.purpose}")
        lines.append(f"  {'':<14} $ {' '.join(step.argv)}")
    return "\n".join(lines)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run a GramDrive automated gate suite and record provenance.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  run_automated.py --suite core --run-id local-core\n"
            "  run_automated.py --suite all --run-id ci-all --require-clean\n"
        ),
    )
    parser.add_argument(
        "--suite",
        help=(
            f"gate suite to run ({', '.join(SUITES)}), or a single step name; "
            f"see --list"
        ),
    )
    parser.add_argument(
        "--run-id",
        help=(
            "names the provenance directory .temp/acceptance/<run-id>; "
            "CI uses ci-<suite>, local runs use local-<suite>"
        ),
    )
    parser.add_argument(
        "--require-clean",
        action="store_true",
        help="refuse to run if the git worktree has uncommitted changes",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="print the suites and steps, then exit",
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(list(argv) if argv is not None else None)

    if args.list:
        print(format_catalog())
        return EXIT_OK

    # Required only once --list is ruled out, so `--list` alone stays usable.
    if not args.suite or not args.run_id:
        parser.error("--suite and --run-id are required (or use --list)")

    if not RUN_ID_RE.match(args.run_id):
        parser.error(
            f"--run-id {args.run_id!r} must be 1-64 chars of letters, digits, "
            f"'.', '_' or '-', starting alphanumeric; it names a directory"
        )

    catalog = build_steps()
    try:
        steps = resolve_suite(args.suite, catalog)
    except KeyError:
        parser.error(
            f"unknown suite {args.suite!r}; suites: {', '.join(SUITES)}; "
            f"steps: {', '.join(catalog)}"
        )

    _, exit_code = run_suite(
        steps,
        repo_root=args.repo_root.resolve(),
        run_id=args.run_id,
        suite=args.suite,
        require_clean=args.require_clean,
    )
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
