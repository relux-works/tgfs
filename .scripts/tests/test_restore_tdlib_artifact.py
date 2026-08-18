#!/usr/bin/env python3
"""Regression tests for runner-local arm64 TDLib restoration."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / ".scripts/tdlib/restore_pinned_artifact.py"


def load_module():
    spec = importlib.util.spec_from_file_location("restore_pinned_artifact", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


restore = load_module()


SYSTEM_DEPENDENCIES = [
    "/usr/lib/libz.1.dylib",
    "/usr/lib/libc++.1.dylib",
    "/usr/lib/libSystem.B.dylib",
]


def tool_runner(*, file_output="Mach-O 64-bit dynamically linked shared library arm64", dependencies=None):
    dependencies = SYSTEM_DEPENDENCIES if dependencies is None else dependencies

    def run(argv):
        if argv[:2] == ("otool", "-L"):
            lines = [
                f"{argv[2]}:",
                f"\t{restore.build_contract.DYLIB_INSTALL_NAME} (compatibility version 1.0.0, current version 1.8.0)",
            ]
            lines.extend(
                f"\t{dependency} (compatibility version 1.0.0, current version 1.0.0)"
                for dependency in dependencies
            )
            return 0, "\n".join(lines) + "\n"
        return 0, f"libtdjson.dylib: {file_output}\n"

    return run


arm64_file = tool_runner()


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
    openssl_license = out / restore.build_contract.OPENSSL_LICENSE_PATH
    openssl_license.parent.mkdir(parents=True)
    openssl_license.write_text(
        "Apache License\nVersion 2.0, January 2004\n", encoding="utf-8"
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
        "third_party": {
            "openssl": {
                "name": restore.build_contract.OPENSSL_NAME,
                "version": restore.build_contract.OPENSSL_VERSION,
                "source": {
                    "url": restore.build_contract.OPENSSL_SOURCE_URL,
                    "sha256": restore.build_contract.OPENSSL_SOURCE_SHA256,
                },
                "build_options": list(restore.build_contract.OPENSSL_BUILD_OPTIONS),
                "license": {
                    "id": restore.build_contract.OPENSSL_LICENSE_ID,
                    "file": restore.build_contract.OPENSSL_LICENSE_PATH.as_posix(),
                    "sha256": checksums[
                        restore.build_contract.OPENSSL_LICENSE_PATH.as_posix()
                    ],
                },
                "linkage": "static",
                "embedded_in": "lib/libtdjson.dylib",
            }
        },
        "toolchain": {"openssl": "OpenSSL 3.6.3 9 Jun 2026"},
        "reproducibility": {"clean_build_tree": True},
        "linkage": SYSTEM_DEPENDENCIES,
        "runtime": {
            "dependency_policy": restore.build_contract.RUNTIME_DEPENDENCY_POLICY,
            "openssl_linkage": "static",
            "dependencies": SYSTEM_DEPENDENCIES,
            "forbidden_builder_paths_verified": True,
            "trust_store": {
                "policy": restore.build_contract.TRUST_STORE_POLICY,
                "cert_file": restore.build_contract.OPENSSL_CERT_FILE,
                "cert_dir": restore.build_contract.OPENSSL_CERT_DIR,
                "config_file": restore.build_contract.OPENSSL_CONFIG_FILE,
                "environment_overrides_scrubbed": True,
                "certificate_objects": 158,
                "verified": True,
            },
        },
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


def rewrite_library(out: Path, payload: bytes) -> None:
    library = out / "lib/libtdjson.dylib"
    library.write_bytes(payload)
    digest = restore.sha256_file(library)
    manifest_path = out / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["artifacts"]["files"]["lib/libtdjson.dylib"]["sha256"] = digest
    manifest["artifacts"]["library"]["sha256"] = digest
    manifest["artifacts"]["library"]["bytes"] = library.stat().st_size
    files = restore.artifact_files(out)
    manifest["artifacts"]["total_bytes"] = sum(path.stat().st_size for path in files.values())
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    checksums = restore.parse_checksums(out / "CHECKSUMS.sha256")
    checksums["lib/libtdjson.dylib"] = digest
    (out / "CHECKSUMS.sha256").write_text(
        "".join(f"{value}  {name}\n" for name, value in sorted(checksums.items())),
        encoding="utf-8",
    )


class RestorePinnedArtifactTests(unittest.TestCase):
    def test_x86_signing_host_can_restore_verified_arm64_artifact(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
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

    def test_openssl_identity_must_match_toolchain_and_license_inventory(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            source = stage_cache(root / "cache")
            manifest_path = source / "manifest.json"
            manifest = json.loads(manifest_path.read_text())
            manifest["third_party"]["openssl"]["version"] = "3.6.4"
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(restore.ArtifactError, "source/build recipe"):
                restore.restore(
                    root / "cache", root / "out", file_runner=arm64_file
                )

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            source = stage_cache(root / "cache")
            (source / restore.build_contract.OPENSSL_LICENSE_PATH).unlink()
            with self.assertRaisesRegex(restore.ArtifactError, "inventory"):
                restore.restore(
                    root / "cache", root / "out", file_runner=arm64_file
                )

    def test_cold_cache_refuses_without_replacing_existing_destination(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            destination = root / "out"
            destination.mkdir()
            marker = destination / "marker"
            marker.write_text("preserved", encoding="utf-8")
            with self.assertRaisesRegex(restore.ColdCacheError, "cache miss"):
                restore.restore(
                    root / "empty-cache", destination, file_runner=arm64_file
                )
            self.assertEqual(marker.read_text(encoding="utf-8"), "preserved")

    def test_symlinked_cache_root_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            real_cache = root / "real-cache"
            stage_cache(real_cache)
            cache_link = root / "cache-link"
            cache_link.symlink_to(real_cache, target_is_directory=True)
            destination = root / "out"
            with self.assertRaisesRegex(
                restore.ArtifactError, "cache root ancestry contains a symlink"
            ):
                restore.restore(cache_link, destination, file_runner=arm64_file)
            self.assertFalse(destination.exists())

    def test_symlinked_cache_root_parent_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            real_parent = root / "real-parent"
            real_cache = real_parent / "cache"
            stage_cache(real_cache)
            parent_link = root / "parent-link"
            parent_link.symlink_to(real_parent, target_is_directory=True)
            destination = root / "out"
            with self.assertRaisesRegex(
                restore.ArtifactError, "cache root ancestry contains a symlink"
            ):
                restore.restore(
                    parent_link / "cache", destination, file_runner=arm64_file
                )
            self.assertFalse(destination.exists())

    def test_symlinked_recipe_key_ancestor_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            real_cache = root / "real-cache"
            stage_cache(real_cache)
            cache = root / "cache"
            cache.mkdir()
            (cache / restore.cache_key()).symlink_to(
                real_cache / restore.cache_key(), target_is_directory=True
            )
            destination = root / "out"
            with self.assertRaisesRegex(
                restore.ArtifactError, "recipe-key directory is a symlink"
            ):
                restore.restore(cache, destination, file_runner=arm64_file)
            self.assertFalse(destination.exists())

    def test_symlinked_artifact_source_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            real_cache = root / "real-cache"
            real_source = stage_cache(real_cache)
            recipe_root = root / "cache" / restore.cache_key()
            recipe_root.mkdir(parents=True)
            (recipe_root / "out").symlink_to(real_source, target_is_directory=True)
            destination = root / "out"
            with self.assertRaisesRegex(
                restore.ArtifactError, "artifact source is a symlink"
            ):
                restore.restore(root / "cache", destination, file_runner=arm64_file)
            self.assertFalse(destination.exists())

    def test_checksum_tamper_is_rejected_before_destination_replacement(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            cache = root / "cache"
            source = stage_cache(cache)
            (source / "lib/libtdjson.dylib").write_bytes(b"tampered")
            destination = root / "out"
            destination.mkdir()
            (destination / "marker").write_text("preserved", encoding="utf-8")
            with self.assertRaisesRegex(restore.ArtifactError, "checksum mismatch"):
                restore.restore(cache, destination, file_runner=arm64_file)
            self.assertTrue((destination / "marker").is_file())

    def test_existing_destination_is_rejected_and_preserved(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            cache = root / "cache"
            stage_cache(cache)
            destination = root / "out"
            destination.mkdir()
            marker = destination / "marker"
            marker.write_text("authoritative", encoding="utf-8")
            with self.assertRaisesRegex(
                restore.ArtifactError, "destination already exists"
            ):
                restore.restore(cache, destination, file_runner=arm64_file)
            self.assertEqual(marker.read_text(encoding="utf-8"), "authoritative")
            self.assertEqual(list(root.glob(".out.restore-*")), [])

    def test_commit_rename_failure_leaves_no_partial_destination_or_residue(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            cache = root / "cache"
            stage_cache(cache)
            destination = root / "out"
            with mock.patch.object(
                Path, "rename", side_effect=OSError("injected commit failure")
            ):
                with self.assertRaisesRegex(
                    restore.ArtifactError, "cannot atomically publish"
                ):
                    restore.restore(cache, destination, file_runner=arm64_file)
            self.assertFalse(destination.exists())
            self.assertEqual(list(root.glob(".out.restore-*")), [])

    def test_wrong_architecture_and_universal_artifact_are_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            with self.assertRaisesRegex(restore.ArtifactError, "not Mach-O arm64"):
                restore.validate_artifact(
                    out, file_runner=tool_runner(file_output="Mach-O 64-bit x86_64")
                )
            with self.assertRaisesRegex(restore.ArtifactError, "not arm64-only"):
                restore.validate_artifact(
                    out, file_runner=tool_runner(file_output="Mach-O universal arm64 x86_64")
                )

    def test_dynamic_homebrew_openssl_is_rejected_even_when_checksummed(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            dependencies = [
                "/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib",
                "/opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib",
                "/usr/lib/libSystem.B.dylib",
            ]
            manifest_path = out / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["linkage"] = dependencies
            manifest["runtime"]["dependencies"] = dependencies
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(restore.ArtifactError, "non-hermetic"):
                restore.validate_artifact(
                    out, file_runner=tool_runner(dependencies=dependencies)
                )

    def test_compiled_homebrew_defaults_are_rejected_with_clean_load_commands(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            rewrite_library(
                out,
                b"arm64 Mach-O\0/opt/homebrew/etc/openssl@3/cert.pem\0",
            )
            with self.assertRaisesRegex(restore.ArtifactError, "builder-local"):
                restore.validate_artifact(out, file_runner=arm64_file)

    def test_runtime_inventory_tamper_is_rejected_against_macho_bytes(self):
        with tempfile.TemporaryDirectory() as raw:
            out = stage_cache(Path(raw) / "cache")
            manifest_path = out / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["runtime"]["dependencies"] = ["/usr/lib/libSystem.B.dylib"]
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(restore.ArtifactError, "inventory disagrees"):
                restore.validate_artifact(out, file_runner=arm64_file)

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
