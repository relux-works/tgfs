#!/usr/bin/env python3
"""Check public names for GramDrive update secrets; never handles values."""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections.abc import Callable, Sequence

EXPECTED = {
    "updates-test": frozenset({"MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "APPSTORE_KEY_ID", "APPSTORE_ISSUER_ID", "APPSTORE_PRIVATE_KEY", "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64"}),
    "release": frozenset({"SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64"}),
}
Runner = Callable[[Sequence[str]], tuple[int, str]]


def run(argv: Sequence[str]) -> tuple[int, str]:
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    except FileNotFoundError:
        return 127, f"{argv[0]} not found"
    return proc.returncode, proc.stdout + proc.stderr


def listed_names(environment: str, runner: Runner = run) -> set[str]:
    code, output = runner(("gh", "secret", "list", "--env", environment, "--json", "name"))
    if code:
        raise RuntimeError(f"cannot list secret names for {environment}: gh exited {code}")
    try:
        return {record["name"] for record in json.loads(output)}
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise RuntimeError(f"invalid name-only response for {environment}") from error


def compare(actual: set[str], expected: frozenset[str]) -> tuple[list[str], list[str]]:
    return sorted(expected - actual), sorted(actual - expected)


def main(argv: Sequence[str] | None = None, runner: Runner = run) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-github", action="store_true", help="compare GitHub environment secret names")
    args = parser.parse_args(argv)
    for environment, names in EXPECTED.items():
        print(f"{environment}: {', '.join(sorted(names))}")
    if not args.check_github:
        print("Name inventory only; no secret values are accepted, read, or written.")
        return 0
    failures: list[str] = []
    for environment, expected in EXPECTED.items():
        try:
            missing, unexpected = compare(listed_names(environment, runner), expected)
        except RuntimeError as error:
            failures.append(str(error))
            continue
        if missing:
            failures.append(f"{environment}: missing {', '.join(missing)}")
        if unexpected:
            failures.append(f"{environment}: unexpected {', '.join(unexpected)}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("GitHub environment secret names match the initial inventory.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
