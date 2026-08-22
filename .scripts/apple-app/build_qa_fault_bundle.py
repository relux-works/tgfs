#!/usr/bin/env python3
"""Build the explicit non-shipping QA File Provider fault-control bundle."""

from __future__ import annotations

import argparse
import importlib.util
import os
from pathlib import Path
import stat
import sys


SCRIPT = Path(__file__).with_name("build_app_bundle.py")
spec = importlib.util.spec_from_file_location("build_app_bundle", SCRIPT)
app = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = app
spec.loader.exec_module(app)


def read_secret(path: Path) -> str:
    facts = path.stat()
    if (
        not stat.S_ISREG(facts.st_mode)
        or facts.st_uid != os.getuid()
        or facts.st_mode & 0o777 != 0o600
    ):
        raise app.StepFailed("QA secret file must be owner-owned regular mode 0600")
    secret = path.read_text(encoding="ascii").strip()
    if len(secret) != 64 or any(character not in "0123456789abcdef" for character in secret):
        raise app.StepFailed("QA secret file must contain exactly 32 lowercase hex bytes")
    return secret


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build a signed or unsigned non-shipping QA fault-control bundle")
    parser.add_argument("--secret-file", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, default=Path(".temp/qa-fault-packaging"))
    parser.add_argument("--core-package", type=Path, default=app.DEFAULT_CORE_PACKAGE)
    parser.add_argument("--identity")
    parser.add_argument("--build-number")
    parser.add_argument("--unsigned", action="store_true")
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(argv)
    if sys.platform != "darwin":
        print("ERROR: QA File Provider bundles require macOS", file=sys.stderr)
        return app.EXIT_CANNOT_START
    try:
        app.package(
            args.repo_root.resolve(),
            out_dir=args.out_dir.resolve(),
            identity=app.resolve_identity(args.identity, dict(os.environ)),
            core_package=args.core_package.resolve(),
            unsigned=args.unsigned,
            build_number=args.build_number,
            qa_fault_secret=read_secret(args.secret_file.resolve()),
        )
    except app.StepFailed as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return app.EXIT_FAILED
    return app.EXIT_OK


if __name__ == "__main__":
    raise SystemExit(main())
