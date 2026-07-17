#!/usr/bin/env python3
"""Verify the active Rust toolchain matches the repo pin (rust-toolchain.toml).

rust-toolchain.toml only binds a build if rustup is the thing driving cargo. A
distro rustc, a Homebrew rust, a container image with a baked-in toolchain, or
`cargo +stable` all ignore it silently and produce a build whose lint set and
rustfmt output are whatever that compiler happens to ship. Every such build
still says "Finished", which is exactly why this check exists: the gate is only
deterministic if something asserts the pin actually took.

Checks, all fatal (exit code 1):
  1. rust-toolchain.toml pins an exact version channel, not a floating one
     ("stable"/"beta"/"nightly" defeat the point of pinning).
  2. The active rustc reports that same version.
  3. The active cargo reports that same version.
  4. Every component listed in rust-toolchain.toml is actually installed and
     runnable (`cargo fmt --version`, `cargo clippy --version`).
  5. Cargo.toml `workspace.package.rust-version` (the MSRV clippy reads) does
     not exceed the pinned channel — an MSRV above the compiler is a claim the
     toolchain cannot honor.
  6. cargo-deny is installed and at least MIN_CARGO_DENY, the version whose
     [advisories] schema deny.toml is written against.

Requires: cargo and rustc on PATH. Python 3.11+ (tomllib).
Usage: python3 .scripts/check_toolchain.py [--repo-root PATH]
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Minimum cargo-deny. 0.18 dropped the [advisories] vulnerability/unsound/
# notice keys and reshaped `unmaintained`; deny.toml uses the newer schema, and
# an older binary rejects it outright rather than degrading quietly.
MIN_CARGO_DENY = (0, 18, 0)

# An exact toolchain pin: 1.91.0. A bare "stable" or a dated nightly is not a
# pin — it resolves differently depending on when and where it runs.
EXACT_CHANNEL_RE = re.compile(r"^\d+\.\d+(?:\.\d+)?$")

# `rustc 1.91.0 (f8297e351 2025-10-28)` / `cargo 1.91.0 (ea2d97820 2025-10-10)`
VERSION_RE = re.compile(r"^\w[\w-]*\s+(\d+\.\d+\.\d+)")

# Component -> the command proving it is installed rather than merely listed.
COMPONENT_PROBES = {
    "rustfmt": ["cargo", "fmt", "--version"],
    "clippy": ["cargo", "clippy", "--version"],
}


def run(argv: list[str], cwd: Path) -> tuple[int, str]:
    """Run argv, returning (exit_code, stdout+stderr). 127 if not on PATH."""
    try:
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True)
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found on PATH"
    return proc.returncode, (proc.stdout + proc.stderr).strip()


def parse_version(output: str) -> str | None:
    match = VERSION_RE.match(output.strip())
    return match.group(1) if match else None


def as_tuple(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def channel_matches(channel: str, actual: str) -> bool:
    """True if `actual` (always x.y.z) satisfies the pinned channel.

    A channel may be written "1.91" or "1.91.0"; both pin the same release, so
    compare only the components the pin actually states.
    """
    pinned = as_tuple(channel)
    return as_tuple(actual)[: len(pinned)] == pinned


def check(repo_root: Path) -> list[str]:
    errors: list[str] = []

    pin_path = repo_root / "rust-toolchain.toml"
    if not pin_path.is_file():
        return [f"{pin_path.name}: missing — the toolchain is not pinned"]

    pin = tomllib.loads(pin_path.read_text(encoding="utf-8")).get("toolchain", {})
    channel = pin.get("channel")
    if not channel:
        return ["rust-toolchain.toml: [toolchain] has no 'channel'"]

    # 1. The pin is exact.
    if not EXACT_CHANNEL_RE.match(channel):
        errors.append(
            f"rust-toolchain.toml: channel '{channel}' is a floating channel; "
            f"pin an exact version (e.g. '1.91.0') so every run compiles with "
            f"the same lint set and rustfmt output"
        )
        return errors

    # 2/3. The active compiler and cargo are the pinned ones.
    for tool in ("rustc", "cargo"):
        code, output = run([tool, "--version"], repo_root)
        if code != 0:
            errors.append(f"{tool}: not runnable ({output})")
            continue
        actual = parse_version(output)
        if actual is None:
            errors.append(f"{tool}: cannot parse version from '{output}'")
        elif not channel_matches(channel, actual):
            errors.append(
                f"{tool}: active version {actual} does not match the "
                f"rust-toolchain.toml pin {channel} — the pin is not in effect "
                f"(is rustup driving cargo, or is a '+toolchain' override set?)"
            )

    # 4. Pinned components are installed.
    for component in pin.get("components", []):
        probe = COMPONENT_PROBES.get(component)
        if probe is None:
            continue
        code, output = run(probe, repo_root)
        if code != 0:
            errors.append(
                f"{component}: pinned in rust-toolchain.toml but not runnable "
                f"({output}) — try `rustup component add {component}`"
            )

    # 5. MSRV cannot exceed the compiler that has to honor it.
    manifest_path = repo_root / "Cargo.toml"
    if manifest_path.is_file():
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        msrv = manifest.get("workspace", {}).get("package", {}).get("rust-version")
        if msrv and as_tuple(msrv) > as_tuple(channel):
            errors.append(
                f"Cargo.toml: workspace.package.rust-version {msrv} is newer "
                f"than the pinned toolchain {channel} — the workspace declares "
                f"support the pinned compiler cannot provide"
            )

    # 6. cargo-deny is present and speaks the deny.toml schema.
    code, output = run(["cargo", "deny", "--version"], repo_root)
    minimum = ".".join(str(part) for part in MIN_CARGO_DENY)
    if code != 0:
        errors.append(
            f"cargo-deny: not installed ({output}) — the supply-chain gate "
            f"(POL-6 licenses, advisories, bans, sources) cannot run; "
            f"install with `brew install cargo-deny`"
        )
    else:
        actual = parse_version(output)
        if actual is None:
            errors.append(f"cargo-deny: cannot parse version from '{output}'")
        elif as_tuple(actual) < MIN_CARGO_DENY:
            errors.append(
                f"cargo-deny {actual} is older than the required {minimum}; "
                f"deny.toml uses the [advisories] schema introduced in "
                f"{minimum} and older releases reject it"
            )

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args()

    errors = check(args.repo_root.resolve())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        print(f"\ntoolchain check FAILED: {len(errors)} error(s)", file=sys.stderr)
        return 1
    print("toolchain check OK: active toolchain matches rust-toolchain.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
