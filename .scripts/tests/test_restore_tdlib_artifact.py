#!/usr/bin/env python3
"""Regression tests for runner-local arm64 TDLib restoration."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / ".scripts/tdlib/restore_pinned_artifact.py"


def load_module():
    spec = importlib.util.spec_from_file_location("restore_pinned_artifact", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


restore = load_module()


def arm64_file(_argv):
    return (
        0,
        "libtdjson.dylib: Mach-O 64-bit dynamically linked shared library arm64\n",
    )


def stage_cache(root: Path) -> Path:
    out = root / restore.cache_key() / "out"
    header = out / "include/td/telegram/td_json_client.h"
    library = out / "lib/libtdjson.dylib"
    header.parent.mkdir(parents=True)
    library.parent.mkdir(parents=True)
    header.write_text("// pinned header\n", encoding="utf-8")
    library.write_bytes(b"arm64 tdjson bytes")
    (out / "LICENSE_1_0.txt").write_text(
        "Boost Software License\n", encoding="utf-8"
    )
    files = restore.artifact_files(out)
    checksums = {name: restore.sha256_file(path) for name, path in files.items()}
    manifest = {
        "schema": restore.build_contract.SCHEMA_VERSION,
        "tool": "build_tdlib.py",
        "gramdrive": {"commit": "a" * 40, "worktree_clean": True},
        "tdlib": {
            "repo": restore.build_contract.TDLIB_REPO,
            "commit": restore.build_contract.TDLIB_COMMIT,
            "runtime_version": "1.8.51",
        },
        "target": {
            "label": restore.build_contract.TARGET_LABEL,
            "arch": restore.build_contract.TARGET_ARCH,
            "macosx_deployment_target": (
                restore.build_contract.MACOSX_DEPLOYMENT_TARGET
            ),
        },
        "license": {"id": restore.build_contract.LICENSE_ID},
        "reproducibility": {"clean_build_tree": True},
        "artifacts": {
            "total_bytes": sum(path.stat().st_size for path in files.values()),
            "files": {
                name: {"sha256": digest} for name, digest in checksums.items()
            },
            "library": {
                "path": "lib/libtdjson.dylib",
                "install_name": restore.build_contract.DYLIB_INSTALL_NAME,
                "sha256": checksums["lib/libtdjson.dylib"],
                "bytes": library.stat().st_size,
            },
        },
    }
    (out / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    (out / "CHECKSUMS.sha256").write_text(
        "".join(
            f"{digest}  {name}\n" for name, digest in sorted(checksums.items())
        ),
        encoding="utf-8",
    )
    return out


class RestorePinnedArtifactTests(unittest.TestCase):
    def test_x86_signing_host_can_restore_verified_arm64_artifact(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            cache = root / "cache"
            source = stage_cache(cache)
            destination = root / "workspace/.temp/tdlib/out"
            manifest = restore.restore(cache, destination, file_runner=arm64_file)
            self.assertEqual(
                (destination / "lib/libtdjson.dylib").read_bytes(),
                (source / "lib/libtdjson.dylib").read_bytes(),
            )
            self.assertEqual(manifest["target"]["arch"], "arm64")
            self.assertNotEqual(manifest["gramdrive"]["commit"], "b" * 40)

    def test_cold_cache_refuses_without_replacing_existing_destination(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            destination = root / "out"
            destination.mkdir()
            marker = destination / "marker"
            marker.write_text("preserved", encoding="utf-8")
            with self.assertRaisesRegex(restore.ColdCacheError, "cache miss"):
                restore.restore(
                    root / "empty-cache", destination, file_runner=arm64_file
                )
            self.assertEqual(marker.read_text(encoding="utf-8"), "preserved")

    def test_checksum_tamper_is_rejected_before_destination_replacement(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            cache = root / "cache"
            source = stage_cache(cache)
            (source / "lib/libtdjson.dylib").write_bytes(b"tampered")
            destination = root / "out"
            destination.mkdir()
            (destination / "marker").write_text("preserved", encoding="utf-8")
            with self.assertRaisesRegex(restore.ArtifactError, "checksum mismatch"):
                restore.restore(cache, destination, file_runner=arm64_file)
            self.assertTrue((destination / "marker").is_file())

    def test_wrong_architecture_and_universal_artifact_are_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            with self.assertRaisesRegex(restore.ArtifactError, "not Mach-O arm64"):
                restore.validate_artifact(
                    out, file_runner=lambda _: (0, "Mach-O 64-bit x86_64")
                )
            with self.assertRaisesRegex(restore.ArtifactError, "not arm64-only"):
                restore.validate_artifact(
                    out, file_runner=lambda _: (0, "Mach-O universal arm64 x86_64")
                )

    def test_manifest_pin_tamper_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            manifest_path = out / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["tdlib"]["commit"] = "f" * 40
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(restore.ArtifactError, "commit is not pinned"):
                restore.validate_artifact(out, file_runner=arm64_file)


if __name__ == "__main__":
    unittest.main()
