#!/usr/bin/env python3
"""Provision public-name update secrets without putting their values in argv."""
from __future__ import annotations

import argparse
import json
import stat
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path
import re

EXPECTED = {
    "updates-test": frozenset({"MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "APPSTORE_KEY_ID", "APPSTORE_ISSUER_ID", "APPSTORE_PRIVATE_KEY", "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64"}),
    "release": frozenset({"SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64"}),
}
INITIAL_ENVIRONMENT = {
    name: environment for environment, names in EXPECTED.items() for name in names
}
SPARKLE_NAME = re.compile(r"^SPARKLE_(TEST|STABLE)_V([1-9][0-9]*)_EDDSA_PRIVATE_KEY_B64$")
DEVELOPER_ID_NAMES = ("MACOS_CERT_P12", "MACOS_CERT_PASSWORD")
NOTARY_NAMES = ("APPSTORE_KEY_ID", "APPSTORE_ISSUER_ID", "APPSTORE_PRIVATE_KEY")
ListRunner = Callable[[Sequence[str]], tuple[int, str]]
SetRunner = Callable[[Sequence[str], bytes], int]


def run(argv: Sequence[str]) -> tuple[int, str]:
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    except FileNotFoundError:
        return 127, f"{argv[0]} not found"
    return proc.returncode, proc.stdout + proc.stderr


def set_run(argv: Sequence[str], value: bytes) -> int:
    """Send a secret only over stdin and deliberately discard command output."""
    try:
        proc = subprocess.run(argv, input=value, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    except FileNotFoundError:
        return 127
    return proc.returncode


def environment_for(name: str) -> str:
    if name in INITIAL_ENVIRONMENT:
        return INITIAL_ENVIRONMENT[name]
    match = SPARKLE_NAME.fullmatch(name)
    if not match:
        raise ValueError(f"unsupported update secret name: {name}")
    return "updates-test" if match.group(1) == "TEST" else "release"


def set_secret(name: str, value: bytes, runner: SetRunner = set_run) -> str:
    """Store non-empty bytes through gh stdin; values are never formatted or reported."""
    environment = environment_for(name)
    if not value:
        raise ValueError(f"refusing empty value for {name}")
    code = runner(("gh", "secret", "set", name, "--env", environment), value)
    if code:
        raise RuntimeError(f"cannot store {name} in {environment}: gh exited {code}")
    return environment


def read_restricted_input(directory: Path, name: str) -> bytes:
    """Read a named secret file only when it is not accessible to group/other."""
    path = directory / name
    file_stat = path.stat(follow_symlinks=False)
    if not stat.S_ISREG(file_stat.st_mode) or stat.S_IMODE(file_stat.st_mode) & 0o077:
        raise ValueError(f"{path} must be a regular owner-only file")
    return path.read_bytes()


def set_group(directory: Path, names: Sequence[str], runner: SetRunner = set_run) -> list[tuple[str, str]]:
    directory_stat = directory.stat(follow_symlinks=False)
    if not stat.S_ISDIR(directory_stat.st_mode) or stat.S_IMODE(directory_stat.st_mode) & 0o077:
        raise ValueError(f"{directory} must be an owner-only directory")
    values = {name: read_restricted_input(directory, name) for name in names}
    return [(name, set_secret(name, values[name], runner)) for name in names]


def listed_names(environment: str, runner: ListRunner = run) -> set[str]:
    code, output = runner(("gh", "secret", "list", "--env", environment, "--json", "name"))
    if code:
        raise RuntimeError(f"cannot list secret names for {environment}: gh exited {code}")
    try:
        return {record["name"] for record in json.loads(output)}
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise RuntimeError(f"invalid name-only response for {environment}") from error


def compare(actual: set[str], expected: frozenset[str]) -> tuple[list[str], list[str]]:
    return sorted(expected - actual), sorted(actual - expected)


def main(argv: Sequence[str] | None = None, runner: ListRunner = run, setter: SetRunner = set_run) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--check-github", action="store_true", help="compare GitHub environment secret names")
    action.add_argument("--set", metavar="NAME", help="read one allowed secret value from stdin and store it")
    action.add_argument("--set-developer-id-from", metavar="DIRECTORY", help="read the Developer ID pair from owner-only files")
    action.add_argument("--set-notary-from", metavar="DIRECTORY", help="read the three notary fields from owner-only files")
    args = parser.parse_args(argv)
    try:
        if args.set:
            environment = set_secret(args.set, sys.stdin.buffer.read(), setter)
            print(f"stored {args.set} in {environment}")
            return 0
        if args.set_developer_id_from:
            stored = set_group(Path(args.set_developer_id_from), DEVELOPER_ID_NAMES, setter)
            print("stored " + ", ".join(f"{name} in {environment}" for name, environment in stored))
            return 0
        if args.set_notary_from:
            stored = set_group(Path(args.set_notary_from), NOTARY_NAMES, setter)
            print("stored " + ", ".join(f"{name} in {environment}" for name, environment in stored))
            return 0
    except (OSError, RuntimeError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    for environment, names in EXPECTED.items():
        print(f"{environment}: {', '.join(sorted(names))}")
    if not args.check_github:
        print("Name inventory only; this invocation accepts, reads, and writes no secret values.")
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
