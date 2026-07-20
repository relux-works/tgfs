#!/usr/bin/env python3
"""Provision the Telegram api credentials for the signed product binaries.

Owned by BUG-260720-3i74u1; the companion of the control-auth smoke. The
dev provisioning convention (TASK-260716-1iypv4) stores api_id/api_hash in
the login keychain under service `gramdrive-telegram` — but an item
created by the `security` CLI is partition-locked to `apple-tool:`, so the
signed gramdrive-agent reading it hangs on an interactive consent prompt
(fatal for the unattended smoke, and one prompt per fresh item for a
human). This script rewrites the two items so both file-keychain gates
pass silently for the product binaries:

  1. compiles and Developer ID-signs the small Swift provisioning tool
     (`provision-telegram-credentials.swift`) with the same identity that
     signs the app bundle — creating the items from a binary of the
     product's team puts the *partition list* on `teamid:<team>`;
  2. runs it with a trusted-application ACL naming the packaged
     gramdrive-agent and GramDrive binaries — covering the *ACL* gate.

Values come from GRAMDRIVE_API_ID/GRAMDRIVE_API_HASH env when set,
otherwise they are captured from the existing keychain items via the
`security` CLI (which may itself prompt once if the items were already
rewritten — pass the env to stay unattended). Secrets never touch argv,
logs, or the repo.

Usage: python3 .scripts/keychain/provision_telegram_credentials.py
           [--identity 'Developer ID Application: ...']
           [--bundle .temp/app-packaging/GramDrive.app]
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
TOOL_SOURCE = Path(__file__).with_name("provision-telegram-credentials.swift")
BUILD_DIR = REPO_ROOT / ".temp" / "keychain-provision"
SERVICE = "gramdrive-telegram"


def fail(message: str) -> "int":
    print(f"provision: {message}", file=sys.stderr)
    return 1


def resolve_identity(explicit: str | None) -> str | None:
    if explicit:
        return explicit
    listing = subprocess.run(
        ["security", "find-identity", "-v", "-p", "codesigning"],
        capture_output=True, text=True, check=False,
    ).stdout
    match = re.search(r'"(Developer ID Application: [^"]+)"', listing)
    return match.group(1) if match else None


def capture_existing(account: str) -> str | None:
    probe = subprocess.run(
        ["security", "find-generic-password", "-s", SERVICE, "-a", account, "-w"],
        capture_output=True, text=True, check=False,
    )
    value = probe.stdout.strip()
    return value if probe.returncode == 0 and value else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--identity", help="Developer ID Application identity name")
    parser.add_argument(
        "--bundle",
        type=Path,
        default=REPO_ROOT / ".temp" / "app-packaging" / "GramDrive.app",
        help="packaged app bundle whose binaries get keychain access",
    )
    args = parser.parse_args()

    agent = args.bundle / "Contents" / "MacOS" / "gramdrive-agent"
    app = args.bundle / "Contents" / "MacOS" / "GramDrive"
    if not agent.is_file() or not app.is_file():
        return fail(f"packaged bundle not found at {args.bundle}; run `make package-app`")

    identity = resolve_identity(args.identity)
    if not identity:
        return fail("no Developer ID Application identity in the keychain")

    api_id = os.environ.get("GRAMDRIVE_API_ID") or capture_existing("api_id")
    api_hash = os.environ.get("GRAMDRIVE_API_HASH") or capture_existing("api_hash")
    if not api_id or not api_hash:
        return fail(
            "api credentials unavailable: set GRAMDRIVE_API_ID/GRAMDRIVE_API_HASH "
            f"or provision the `{SERVICE}` keychain items first"
        )

    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    tool = BUILD_DIR / "provision-telegram-credentials"
    compile_run = subprocess.run(
        ["swiftc", "-O", "-o", str(tool), str(TOOL_SOURCE)],
        capture_output=True, text=True, check=False,
    )
    if compile_run.returncode != 0:
        return fail(f"tool compile failed:\n{compile_run.stderr}")
    sign_run = subprocess.run(
        ["codesign", "--force", "--sign", identity, "--timestamp", str(tool)],
        capture_output=True, text=True, check=False,
    )
    if sign_run.returncode != 0:
        return fail(f"tool signing failed:\n{sign_run.stderr}")

    # Prove the freshly signed tool actually loads and runs on THIS machine
    # (Gatekeeper, quarantine, a bad link) before we clear the existing
    # keychain items — a tool that cannot run must not first destroy the only
    # copies of the credentials.
    check_run = subprocess.run(
        [str(tool), "--check"], capture_output=True, text=True, check=False,
    )
    if check_run.returncode != 0:
        return fail(f"provisioning tool is not runnable:\n{check_run.stderr}")

    # Items created by the `security` CLI refuse a delete from the signed
    # tool (errSecInvalidOwnerEdit); their creator removes them. Items the
    # tool itself created earlier are replaced by the tool.
    for account in ("api_id", "api_hash"):
        subprocess.run(
            ["security", "delete-generic-password", "-s", SERVICE, "-a", account],
            capture_output=True, check=False,
        )

    environment = dict(os.environ)
    environment["GRAMDRIVE_API_ID"] = api_id
    environment["GRAMDRIVE_API_HASH"] = api_hash
    provision = subprocess.run(
        [str(tool), "--agent", str(agent), "--app", str(app)],
        env=environment, check=False,
    )
    if provision.returncode != 0:
        return fail("provisioning tool failed")
    print(f"provisioned: service `{SERVICE}` readable by {agent.name} and {app.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
