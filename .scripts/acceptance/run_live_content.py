#!/usr/bin/env python3
"""Run the privacy-safe synthetic live-content acceptance matrix.

The product components own their focused fixtures and assertions. This runner
composes those accepted Rust and Swift suites into one pre-install gate and
writes only aggregate, allow-listed evidence. Child stdout/stderr is discarded:
test output is useful at a terminal, but it is not an acceptable place to
persist chat content.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_CANNOT_START = 2

HARNESS_VERSION = "1.0.0"
FIXTURE_VERSION = "synthetic-live-content-v1"
SCENARIO_DEADLINE_SECONDS = 900
EVIDENCE_BYTE_LIMIT = 64 * 1024


@dataclass(frozen=True)
class Scenario:
    """One fixed synthetic gate leg."""

    label: str
    argv: tuple[str, ...]


@dataclass(frozen=True)
class ScenarioResult:
    """Privacy-safe result retained for one gate leg."""

    label: str
    passed: bool
    timed_out: bool
    exit_code: int
    duration_ms: int


def build_catalog() -> tuple[Scenario, ...]:
    """Return the fixed cross-language acceptance matrix."""
    return (
        Scenario(
            "rust-history-live-stories",
            (
                "cargo",
                "test",
                "-p",
                "gramdrive-source-tdjson",
                "--test",
                "history_crawl",
                "--test",
                "live_updates",
                "--test",
                "story_discovery",
            ),
        ),
        Scenario(
            "rust-monthly-render",
            (
                "cargo",
                "test",
                "-p",
                "gramdrive-engine",
                "--test",
                "backfill_scheduler",
                "--test",
                "render_pipeline",
                "--test",
                "render_plan",
            ),
        ),
        Scenario(
            "rust-markdown-ndjson",
            ("cargo", "test", "-p", "gramdrive-render"),
        ),
        Scenario(
            "rust-state-fidelity-retention-scale",
            (
                "cargo",
                "test",
                "-p",
                "gramdrive-state",
                "--test",
                "repo_changes",
                "--test",
                "repo_content_progress",
                "--test",
                "repo_live_content",
                "--test",
                "repo_retention",
                "--test",
                "query_plans",
            ),
        ),
        Scenario(
            "rust-ffi-hydration-policy",
            ("cargo", "test", "-p", "gramdrive-ffi", "--lib"),
        ),
        Scenario(
            "swift-package-build",
            ("swift", "build", "--package-path", "apple/GramDriveSupport"),
        ),
        Scenario(
            "swift-provider-companion-regressions",
            ("swift", "test", "--package-path", "apple/GramDriveSupport"),
        ),
    )


Runner = Callable[[Sequence[str], Path, int], tuple[int, bool, int]]


def _signal_process(process: subprocess.Popen, value: signal.Signals) -> None:
    """Signal the whole child group; tolerate a process that just exited."""
    try:
        if os.name == "posix":
            os.killpg(process.pid, value)
        elif value == signal.SIGTERM:
            process.terminate()
        else:
            process.kill()
    except ProcessLookupError:
        pass


def default_runner(
    argv: Sequence[str], repo_root: Path, deadline_seconds: int
) -> tuple[int, bool, int]:
    """Run one leg with constant-output memory and a hard wall-clock bound."""
    started = time.monotonic()
    try:
        process = subprocess.Popen(
            list(argv),
            cwd=repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except FileNotFoundError:
        return 127, False, round((time.monotonic() - started) * 1000)

    timed_out = False
    try:
        exit_code = process.wait(timeout=deadline_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        _signal_process(process, signal.SIGTERM)
        try:
            exit_code = process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _signal_process(process, signal.SIGKILL)
            exit_code = process.wait()
        exit_code = 124
    duration_ms = round((time.monotonic() - started) * 1000)
    return exit_code, timed_out, duration_ms


def tool_versions(repo_root: Path) -> dict[str, str]:
    """Capture bounded first-line tool versions; no project output is read."""
    probes = {
        "python": (sys.executable, "--version"),
        "cargo": ("cargo", "--version"),
        "rustc": ("rustc", "--version"),
        "swift": ("swift", "--version"),
    }
    versions: dict[str, str] = {}
    for label, argv in probes.items():
        try:
            result = subprocess.run(
                argv,
                cwd=repo_root,
                capture_output=True,
                text=True,
                timeout=10,
            )
            output = (result.stdout + result.stderr)[:4096]
            first = output.strip().splitlines()[0] if output.strip() else "unavailable"
            versions[label] = first[:256] if result.returncode == 0 else "unavailable"
        except (FileNotFoundError, subprocess.TimeoutExpired):
            versions[label] = "unavailable"
    return versions


def validate_catalog(catalog: Sequence[Scenario]) -> None:
    labels = [scenario.label for scenario in catalog]
    expected = [scenario.label for scenario in build_catalog()]
    if labels != expected:
        raise ValueError(f"scenario labels must be the fixed catalog: {expected}")
    if any(not scenario.argv for scenario in catalog):
        raise ValueError("every scenario must carry one command")


def validate_evidence(evidence: dict, catalog: Sequence[Scenario]) -> None:
    """Reject free-form evidence fields and non-catalog labels."""
    expected_top = {
        "schema_version",
        "harness_version",
        "fixture_version",
        "synthetic_only",
        "privacy_safe",
        "passed",
        "scenario_count",
        "passed_count",
        "failed_count",
        "deadline_seconds",
        "evidence_byte_limit",
        "evidence_byte_count",
        "evidence_within_bound",
        "versions",
        "scenarios",
    }
    if set(evidence) != expected_top:
        raise ValueError("live-content evidence contains a non-allow-listed top-level field")
    if set(evidence["versions"]) != {"python", "cargo", "rustc", "swift"}:
        raise ValueError("live-content evidence contains a non-allow-listed version field")

    expected_scenario = {
        "label",
        "passed",
        "timed_out",
        "exit_code",
        "duration_ms",
    }
    labels = [scenario.label for scenario in catalog]
    recorded = evidence["scenarios"]
    if [result["label"] for result in recorded] != labels:
        raise ValueError("live-content evidence contains a non-catalog scenario label")
    if any(set(result) != expected_scenario for result in recorded):
        raise ValueError("live-content evidence contains a non-allow-listed scenario field")
    if evidence["fixture_version"] != FIXTURE_VERSION:
        raise ValueError("live-content evidence must use the fixed synthetic fixture label")
    if evidence["synthetic_only"] is not True or evidence["privacy_safe"] is not True:
        raise ValueError("live-content evidence must remain synthetic and privacy-safe")


def encode_bounded(evidence: dict, catalog: Sequence[Scenario]) -> bytes:
    """Set the exact byte count, validate the schema, and enforce its bound."""
    evidence["evidence_byte_count"] = 0
    for _ in range(4):
        encoded = (json.dumps(evidence, indent=2, sort_keys=True) + "\n").encode()
        if evidence["evidence_byte_count"] == len(encoded):
            break
        evidence["evidence_byte_count"] = len(encoded)
    encoded = (json.dumps(evidence, indent=2, sort_keys=True) + "\n").encode()
    evidence["evidence_within_bound"] = len(encoded) <= EVIDENCE_BYTE_LIMIT
    encoded = (json.dumps(evidence, indent=2, sort_keys=True) + "\n").encode()
    evidence["evidence_byte_count"] = len(encoded)
    encoded = (json.dumps(evidence, indent=2, sort_keys=True) + "\n").encode()
    validate_evidence(evidence, catalog)
    if len(encoded) > EVIDENCE_BYTE_LIMIT:
        raise ValueError(
            f"live-content evidence is {len(encoded)} bytes; limit is {EVIDENCE_BYTE_LIMIT}"
        )
    return encoded


def run_acceptance(
    *,
    repo_root: Path,
    output: Path,
    catalog: Sequence[Scenario],
    runner: Runner = default_runner,
    versions: dict[str, str] | None = None,
    echo: Callable[[str], None] = print,
) -> tuple[dict, int]:
    """Run all legs and write the bounded privacy-safe evidence record."""
    validate_catalog(catalog)
    results: list[ScenarioResult] = []
    for scenario in catalog:
        echo(f"==> {scenario.label}")
        exit_code, timed_out, duration_ms = runner(
            scenario.argv, repo_root, SCENARIO_DEADLINE_SECONDS
        )
        passed = exit_code == 0 and not timed_out
        results.append(
            ScenarioResult(
                label=scenario.label,
                passed=passed,
                timed_out=timed_out,
                exit_code=exit_code,
                duration_ms=duration_ms,
            )
        )
        echo(
            f"<== {scenario.label}: {'ok' if passed else 'FAILED'} "
            f"(exit {exit_code}, {duration_ms} ms)"
        )

    passed_count = sum(result.passed for result in results)
    evidence = {
        "schema_version": 1,
        "harness_version": HARNESS_VERSION,
        "fixture_version": FIXTURE_VERSION,
        "synthetic_only": True,
        "privacy_safe": True,
        "passed": passed_count == len(results),
        "scenario_count": len(results),
        "passed_count": passed_count,
        "failed_count": len(results) - passed_count,
        "deadline_seconds": SCENARIO_DEADLINE_SECONDS,
        "evidence_byte_limit": EVIDENCE_BYTE_LIMIT,
        "evidence_byte_count": 0,
        "evidence_within_bound": True,
        "versions": versions if versions is not None else tool_versions(repo_root),
        "scenarios": [
            {
                "label": result.label,
                "passed": result.passed,
                "timed_out": result.timed_out,
                "exit_code": result.exit_code,
                "duration_ms": result.duration_ms,
            }
            for result in results
        ],
    }
    encoded = encode_bounded(evidence, catalog)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(encoded)
    return evidence, EXIT_OK if evidence["passed"] else EXIT_FAILED


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(list(argv) if argv is not None else None)

    catalog = build_catalog()
    if args.list:
        for scenario in catalog:
            print(scenario.label)
        return EXIT_OK
    if args.output is None:
        parser.error("--output is required (or use --list)")
    try:
        _, exit_code = run_acceptance(
            repo_root=args.repo_root.resolve(),
            output=args.output.resolve(),
            catalog=catalog,
        )
    except ValueError as error:
        print(f"ERROR: {error}")
        return EXIT_CANNOT_START
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
