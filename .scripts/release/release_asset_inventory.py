#!/usr/bin/env python3
"""Capture and query one GitHub Release asset inventory without streaming probes."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


class AssetInventoryError(ValueError):
    """The Release asset inventory cannot support an immutable decision."""


def validate_name(value: object) -> str:
    if not isinstance(value, str) or not value or value != value.strip():
        raise AssetInventoryError("Release asset name must be a nonempty exact string")
    if value in (".", "..") or "/" in value or "\\" in value:
        raise AssetInventoryError(f"unsafe Release asset name: {value!r}")
    if any(ord(character) < 32 or ord(character) == 127 for character in value):
        raise AssetInventoryError("Release asset name contains a control character")
    return value


def normalize_inventory(payload: object, *, captured: bool) -> dict[str, list[dict[str, str]]]:
    if not isinstance(payload, dict) or set(payload) != {"assets"}:
        raise AssetInventoryError("Release asset inventory must be an exact assets object")
    assets = payload["assets"]
    if not isinstance(assets, list):
        raise AssetInventoryError("Release assets must be an array")
    names: list[str] = []
    for asset in assets:
        if not isinstance(asset, dict):
            raise AssetInventoryError("Release asset record must be an object")
        if not captured and set(asset) != {"name"}:
            raise AssetInventoryError("normalized Release asset record must contain only name")
        name = validate_name(asset.get("name"))
        if name in names:
            raise AssetInventoryError(f"duplicate Release asset name: {name}")
        names.append(name)
    return {"assets": [{"name": name} for name in names]}


def read_inventory(path: Path) -> dict[str, list[dict[str, str]]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssetInventoryError(f"cannot read Release asset inventory: {error}") from error
    return normalize_inventory(payload, captured=False)


def capture(release: str, output: Path) -> None:
    validate_name(release)
    result = subprocess.run(
        ["gh", "release", "view", release, "--json", "assets"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or "no diagnostic"
        raise AssetInventoryError(
            f"gh release view failed with exit {result.returncode}: {detail}"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise AssetInventoryError("gh release view returned malformed JSON") from error
    normalized = normalize_inventory(payload, captured=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_name = ""
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=output.parent,
            prefix=f".{output.name}.",
            delete=False,
        ) as temporary:
            temporary_name = temporary.name
            json.dump(normalized, temporary, sort_keys=True, separators=(",", ":"))
            temporary.write("\n")
        os.replace(temporary_name, output)
    finally:
        if temporary_name:
            try:
                Path(temporary_name).unlink()
            except FileNotFoundError:
                pass


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    capture_parser = subparsers.add_parser("capture")
    capture_parser.add_argument("--release", required=True)
    capture_parser.add_argument("--output", required=True, type=Path)
    state_parser = subparsers.add_parser("state")
    state_parser.add_argument("--inventory", required=True, type=Path)
    state_parser.add_argument("--name", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "capture":
            capture(args.release, args.output)
            return 0
        name = validate_name(args.name)
        inventory = read_inventory(args.inventory)
        names = [asset["name"] for asset in inventory["assets"]]
        print("present" if name in names else "absent")
        return 0
    except AssetInventoryError as error:
        print(f"release asset inventory: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
