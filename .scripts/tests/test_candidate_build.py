#!/usr/bin/env python3
"""Regression tests for the candidate signing/notarization trust boundary."""
from __future__ import annotations

import argparse
import base64
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from hashlib import sha256
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def load(name: str, relative: str):
    spec = importlib.util.spec_from_file_location(name, REPO / relative)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


candidate = load("build_candidate_package", ".scripts/release/build_candidate_package.py")
order = load("check_candidate_build_order", ".scripts/release/check_candidate_build_order.py")
release_selection = load(
    "select_stable_release", ".scripts/release/select_stable_release.py"
)
COMMIT = "a" * 40
DMG = b"exact candidate dmg bytes"
DMG_SHA = sha256(DMG).hexdigest()
TDLIB_SHA = sha256(b"tdlib").hexdigest()
SIGNED_TDLIB_SHA = sha256(b"signed tdlib").hexdigest()
CORE_SHA = sha256(b"core").hexdigest()
CORE_ZIP_SHA = sha256(b"core zip").hexdigest()
OPENSSL_LICENSE = b"Apache License 2.0\n"
OPENSSL_LICENSE_SHA = sha256(OPENSSL_LICENSE).hexdigest()


def openssl_attribution(license_path: str) -> dict:
    return {
        "name": "OpenSSL",
        "version": "3.6.3",
        "source": {
            "url": "https://example.invalid/openssl-3.6.3.tar.gz",
            "sha256": candidate.PINNED_OPENSSL_SOURCE_SHA256,
        },
        "build_options": ["no-shared", "no-pinshared", "no-module"],
        "linkage": "static",
        "embedded_in": "lib/libtdjson.dylib",
        "license": {
            "id": "Apache-2.0",
            "file": license_path,
            "sha256": OPENSSL_LICENSE_SHA,
        },
    }


def tdlib_runtime() -> dict:
    return {
        "dependency_policy": "system-only-static-openssl",
        "openssl_linkage": "static",
        "dependencies": ["/usr/lib/libSystem.B.dylib"],
        "forbidden_builder_paths_verified": True,
        "trust_store": {
            "policy": "macos-system-pem",
            "cert_file": candidate.OPENSSL_CERT_FILE,
            "cert_dir": "/etc/ssl/certs",
            "config_file": "/etc/ssl/openssl.cnf",
            "environment_overrides_scrubbed": True,
            "certificate_objects": 158,
            "verified": True,
        },
    }


def write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value), encoding="utf-8")


def args(root: Path, mode: str = "test") -> argparse.Namespace:
    return argparse.Namespace(
        app_dir=str(root / "app"), core_dir=str(root / "core"), tdlib_dir=str(root / "tdlib"),
        out_dir=str(root / "candidate"), mode=mode, commit=COMMIT, minimum_build=136,
        repository="relux-works/tgfs", ref="refs/heads/main",
        workflow_ref="relux-works/tgfs/.github/workflows/candidate-build.yml@refs/heads/main",
        run_id="123", run_attempt="1",
    )


def stage(root: Path, mode: str = "test") -> None:
    app_dir, core_dir, tdlib_dir = root / "app", root / "core", root / "tdlib"
    for directory in (app_dir, core_dir, tdlib_dir):
        directory.mkdir()
    (app_dir / "GramDrive-0.5.0.dmg").write_bytes(DMG)
    embedded_tdlib = app_dir / "GramDrive.app" / "Contents" / "Frameworks" / "libtdjson.dylib"
    embedded_tdlib.parent.mkdir(parents=True)
    embedded_tdlib.write_bytes(b"signed tdlib")
    app_openssl_license = (
        app_dir / "GramDrive.app" / "Contents" / "Resources" / candidate.OPENSSL_LICENSE_PATH
    )
    app_openssl_license.parent.mkdir(parents=True)
    app_openssl_license.write_bytes(OPENSSL_LICENSE)
    app_checksums = {
        "GramDrive-0.5.0.dmg": DMG_SHA,
        "GramDrive.app/Contents/Frameworks/libtdjson.dylib": SIGNED_TDLIB_SHA,
        candidate.APP_OPENSSL_LICENSE_PATH: OPENSSL_LICENSE_SHA,
    }
    (app_dir / "CHECKSUMS.sha256").write_text(
        candidate.render_checksums(app_checksums.items())
    )
    app = {
        "schema": 1, "signed": True, "git": {"commit": COMMIT, "worktree_clean": True},
        "platform": "macos-arm64", "binary_arch": {"required": "arm64"},
        "sparkle": {"channel": "test" if mode == "test" else "stable"},
        "team_id": candidate.TEAM_ID, "signing_identity": candidate.IDENTITY,
        "binaries": [{"bundle_id": name, "cdhash": "deadbeef"} for name in candidate.EXPECTED_BUNDLE_IDS],
        "notarization": {"submitted": True, "status": "Accepted", "app": {"status": "Accepted"}, "dmg": {"status": "Accepted"}},
        "signature_verification": {"app": "passed", "dmg": "passed", "nested": "passed"},
        "staple_verification": {"app": "validated", "dmg": "validated"},
        "gatekeeper": {"app": "accepted", "dmg": "accepted"},
        "shipped_code_verification": {
            "complete": True,
            "required_architecture": "arm64",
            "expected_team_id": candidate.TEAM_ID,
            "expected_authority": candidate.IDENTITY,
            "count": 1,
            "objects": [{
                "path": "Contents/Frameworks/libtdjson.dylib",
                "architectures": ["arm64"],
                "signature": "passed",
                "team_id": candidate.TEAM_ID,
                "authority": candidate.IDENTITY,
            }],
        },
        "tdjson": {
            "linked": True,
            "embedded_libraries": ["libtdjson.dylib"],
            "signing_transition": {
                "required": True,
                "operation": "developer-id-codesign",
                "source": {"artifact": "staged-core", "sha256": TDLIB_SHA},
                "pre_sign": {
                    "bundle_path": "Contents/Frameworks/libtdjson.dylib",
                    "sha256": TDLIB_SHA,
                    "matches_source": True,
                },
                "post_sign": {
                    "bundle_path": "Contents/Frameworks/libtdjson.dylib",
                    "sha256": SIGNED_TDLIB_SHA,
                },
                "signature": {
                    "verified": True,
                    "team_id": candidate.TEAM_ID,
                    "authority": candidate.IDENTITY,
                    "architecture": "arm64",
                },
            },
            "runtime": {
                "verified": True,
                "dependency_policy": "system-or-bundle-relative-static-openssl",
                "openssl_linkage": "static",
                "dependencies": ["/usr/lib/libSystem.B.dylib"],
                "forbidden_builder_paths_verified": True,
                "trust_store": tdlib_runtime()["trust_store"],
            },
        },
        "third_party": {
            "openssl": openssl_attribution(
                f"Contents/Resources/{candidate.OPENSSL_LICENSE_PATH}"
            )
        },
        "product_version": {
            "short": "0.5.0",
            "build": "137",
            "git_build_floor": "136",
            "build_source": "reviewed-workflow-override",
        },
        "checksums": app_checksums, "sizes": {"dmg_bytes": len(DMG)},
        "toolchain": {"swift": "Swift test"},
    }
    core_artifact = core_dir / "GramDriveCore"
    core_artifact.mkdir()
    (core_artifact / "core.bin").write_bytes(b"core")
    (core_artifact / "lib").mkdir()
    (core_artifact / "lib" / "libtdjson.dylib").write_bytes(b"tdlib")
    core_openssl_license = core_artifact / candidate.OPENSSL_LICENSE_PATH
    core_openssl_license.parent.mkdir(parents=True)
    core_openssl_license.write_bytes(OPENSSL_LICENSE)
    core_zip = core_dir / "GramDriveCore-0.5.0.zip"
    core_zip.write_bytes(b"core zip")
    core_checksums = {
        "core.bin": CORE_SHA,
        "lib/libtdjson.dylib": TDLIB_SHA,
        candidate.OPENSSL_LICENSE_PATH: OPENSSL_LICENSE_SHA,
        "../GramDriveCore-0.5.0.zip": CORE_ZIP_SHA,
    }
    core = {
        "git": {"commit": COMMIT, "worktree_clean": True},
        "tdjson": {
            "linked": True,
            "library_sha256": TDLIB_SHA,
            "runtime": tdlib_runtime(),
        },
        "host_test_slice": None, "toolchain": {"rustc": "rustc test"}, "checksums": core_checksums,
        "third_party": {
            "openssl": openssl_attribution(candidate.OPENSSL_LICENSE_PATH)
        },
    }
    (tdlib_dir / "lib").mkdir()
    (tdlib_dir / "lib" / "libtdjson.dylib").write_bytes(b"tdlib")
    tdlib_openssl_license = tdlib_dir / candidate.OPENSSL_LICENSE_PATH
    tdlib_openssl_license.parent.mkdir(parents=True)
    tdlib_openssl_license.write_bytes(OPENSSL_LICENSE)
    tdlib_checksums = {
        "lib/libtdjson.dylib": TDLIB_SHA,
        candidate.OPENSSL_LICENSE_PATH: OPENSSL_LICENSE_SHA,
    }
    tdlib = {
        "gramdrive": {"commit": "c" * 40, "worktree_clean": True},
        "tdlib": {"repo": candidate.PINNED_TDLIB_REPO, "commit": candidate.PINNED_TDLIB_COMMIT, "runtime_version": "1.8.51"},
        "target": {"label": "macos-arm64", "arch": "arm64"},
        "reproducibility": {"clean_build_tree": True},
        "linkage": ["/usr/lib/libSystem.B.dylib"],
        "runtime": tdlib_runtime(),
        "third_party": {
            "openssl": openssl_attribution(candidate.OPENSSL_LICENSE_PATH)
        },
        "artifacts": {
            "library": {"sha256": TDLIB_SHA},
            "files": {name: {"sha256": digest} for name, digest in tdlib_checksums.items()},
        },
        "toolchain": {"cmake": "cmake test"},
    }
    write_json(app_dir / "manifest.json", app)
    write_json(core_dir / "manifest.json", core)
    write_json(tdlib_dir / "manifest.json", tdlib)
    (core_dir / "CHECKSUMS.sha256").write_text(candidate.render_checksums(core_checksums.items()))
    (tdlib_dir / "CHECKSUMS.sha256").write_text(candidate.render_checksums(tdlib_checksums.items()))


def attestation_bundle(subjects: dict[str, str]) -> dict:
    statement = {"_type": "https://in-toto.io/Statement/v1", "subject": [{"name": name, "digest": {"sha256": digest}} for name, digest in subjects.items()]}
    payload = base64.b64encode(json.dumps(statement).encode()).decode()
    return {"mediaType": "application/vnd.dev.sigstore.bundle+json;version=0.3", "dsseEnvelope": {"payload": payload}}


class CandidatePackageTests(unittest.TestCase):
    def test_compiled_homebrew_defaults_are_rejected_without_load_commands(self):
        with tempfile.TemporaryDirectory() as raw:
            library = Path(raw) / "libtdjson.dylib"
            library.write_bytes(
                b"clean Mach-O loads\0/opt/homebrew/etc/openssl@3/cert.pem\0"
            )
            with self.assertRaisesRegex(candidate.CandidateError, "builder-local"):
                candidate.require_portable_tdlib_bytes(library, "signed app")

    def test_build_finalize_verify_preserves_exact_dmg_and_binds_attestation(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            manifest = candidate.build_candidate(args(root))
            out = root / "candidate"
            self.assertEqual(manifest["dmg"]["name"], "GramDrive-0.5.0-137.dmg")
            self.assertEqual(manifest["dmg"]["packaging_name"], "GramDrive-0.5.0.dmg")
            self.assertEqual((out / manifest["dmg"]["name"]).read_bytes(), DMG)
            self.assertFalse(manifest["publication"]["owned"])
            self.assertEqual(candidate.read_json(out / "verification.json")["gates"]["privacy_scrub"], "passed")
            subjects = candidate.parse_checksums(out / "SUBJECTS.sha256")
            bundle_path = root / "attestation.json"
            write_json(bundle_path, attestation_bundle(subjects))
            candidate.finalize_candidate(argparse.Namespace(out_dir=str(out), attestation_bundle=str(bundle_path)))
            verified = candidate.verify_candidate(argparse.Namespace(out_dir=str(out)))
            self.assertEqual(verified["dmg"]["sha256"], DMG_SHA)
            self.assertEqual(candidate.read_json(out / "finalization.json")["status"], "verified-and-attested")
            provenance = candidate.read_json(out / "candidate-provenance.json")
            self.assertEqual(provenance["tdlib"]["library_sha256"], TDLIB_SHA)
            self.assertEqual(provenance["tdlib"]["builder_source_commit"], "c" * 40)
            self.assertEqual(provenance["tdlib"]["runtime"]["openssl_linkage"], "static")
            self.assertEqual(
                provenance["tdlib"]["third_party"]["openssl"]["version"], "3.6.3"
            )
            self.assertEqual(
                provenance["app_third_party"]["openssl"]["license"]["sha256"],
                OPENSSL_LICENSE_SHA,
            )
            self.assertTrue(provenance["app_runtime"]["verified"])
            self.assertEqual(
                provenance["app_tdlib_signing_transition"]["post_sign"]["sha256"],
                SIGNED_TDLIB_SHA,
            )

            verification = candidate.read_json(out / "verification.json")
            verification["result"] = "forged"
            write_json(out / "verification.json", verification)
            outer = candidate.parse_checksums(out / "CANDIDATE-CHECKSUMS.sha256")
            outer["verification.json"] = candidate.sha256_file(out / "verification.json")
            (out / "CANDIDATE-CHECKSUMS.sha256").write_text(candidate.render_checksums(outer.items()))
            with self.assertRaisesRegex(candidate.CandidateError, "attested candidate subject changed"):
                candidate.verify_candidate(argparse.Namespace(out_dir=str(out)))

    def test_stable_candidate_requires_the_stable_embedded_channel(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root, mode="test")
            with self.assertRaisesRegex(candidate.CandidateError, "Sparkle channel"):
                candidate.build_candidate(args(root, mode="stable-candidate"))

    def test_rejects_unverified_dmg_or_non_live_core(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app = candidate.read_json(root / "app" / "manifest.json")
            app["staple_verification"]["dmg"] = "not-run"
            write_json(root / "app" / "manifest.json", app)
            with self.assertRaisesRegex(candidate.CandidateError, "staple"):
                candidate.build_candidate(args(root))

            second = Path(raw) / "second"
            second.mkdir()
            stage(second)
            embedded = second / "app" / "GramDrive.app" / "Contents" / "Frameworks" / "libtdjson.dylib"
            embedded.write_bytes(b"different tdlib")
            embedded_sha = candidate.sha256_file(embedded)
            checksums = second / "app" / "CHECKSUMS.sha256"
            checksums.write_text(checksums.read_text().replace(TDLIB_SHA, embedded_sha))
            app_manifest = candidate.read_json(second / "app" / "manifest.json")
            app_manifest["checksums"]["GramDrive.app/Contents/Frameworks/libtdjson.dylib"] = embedded_sha
            write_json(second / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "checksum mismatch"):
                candidate.build_candidate(args(second))

    def test_rejects_forged_pre_sign_tdlib_provenance(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["tdjson"]["signing_transition"]["pre_sign"]["sha256"] = "0" * 64
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "pre-sign provenance"):
                candidate.build_candidate(args(root))

    def test_rejects_post_sign_bytes_not_bound_to_final_app_checksum(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["tdjson"]["signing_transition"]["post_sign"]["sha256"] = "0" * 64
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "post-sign provenance"):
                candidate.build_candidate(args(root))

    def test_rejects_signed_transition_without_tdlib_code_readback(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["shipped_code_verification"]["objects"][0]["path"] = (
                "Contents/MacOS/other"
            )
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "exactly one shipped-code"):
                candidate.build_candidate(args(root))

    def test_rejects_unverified_or_dynamic_openssl_runtime_provenance(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            tdlib_manifest = candidate.read_json(root / "tdlib" / "manifest.json")
            tdlib_manifest["runtime"] = {
                "dependency_policy": "system-only-static-openssl",
                "openssl_linkage": "dynamic",
                "dependencies": ["/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib"],
            }
            tdlib_manifest["linkage"] = tdlib_manifest["runtime"]["dependencies"]
            write_json(root / "tdlib" / "manifest.json", tdlib_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "static-OpenSSL"):
                candidate.build_candidate(args(root))

    def test_rejects_tampered_or_missing_openssl_attribution(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            license_path = root / "core" / "GramDriveCore" / candidate.OPENSSL_LICENSE_PATH
            license_path.write_text("tampered attribution\n")
            with self.assertRaisesRegex(candidate.CandidateError, "checksum mismatch"):
                candidate.build_candidate(args(root))
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            del app_manifest["third_party"]
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "app OpenSSL attribution"):
                candidate.build_candidate(args(root))

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["tdjson"]["runtime"]["verified"] = False
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "runtime dependency closure"):
                candidate.build_candidate(args(root))

    def test_requires_complete_nested_code_readback(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["shipped_code_verification"]["complete"] = False
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "readback is incomplete"):
                candidate.build_candidate(args(root))

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["shipped_code_verification"]["objects"][0]["team_id"] = "BADTEAM123"
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "shipped code object failed"):
                candidate.build_candidate(args(root))

    def test_checksum_inventories_reject_tamper_missing_and_extra_entries(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            (root / "core" / "GramDriveCore" / "core.bin").write_bytes(b"tampered")
            with self.assertRaisesRegex(candidate.CandidateError, "checksum mismatch: core.bin"):
                candidate.build_candidate(args(root))

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_checksums = root / "app" / "CHECKSUMS.sha256"
            app_checksums.write_text(
                "\n".join(line for line in app_checksums.read_text().splitlines() if "libtdjson" not in line) + "\n"
            )
            with self.assertRaisesRegex(candidate.CandidateError, "checksum inventory mismatch"):
                candidate.build_candidate(args(root))

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            tdlib_checksums = root / "tdlib" / "CHECKSUMS.sha256"
            tdlib_checksums.write_text(tdlib_checksums.read_text() + f"{'0' * 64}  ghost.bin\n")
            with self.assertRaisesRegex(candidate.CandidateError, "checksum inventory mismatch"):
                candidate.build_candidate(args(root))

    def test_rejects_non_monotonic_build_and_attestation_subject_drift(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            build_args = args(root); build_args.minimum_build = 137
            with self.assertRaisesRegex(candidate.CandidateError, "not newer"):
                candidate.build_candidate(build_args)
            build_args.minimum_build = 136
            candidate.build_candidate(build_args)
            bundle_path = root / "attestation.json"
            write_json(bundle_path, attestation_bundle({"GramDrive-0.5.0.dmg": "0" * 64}))
            with self.assertRaisesRegex(candidate.CandidateError, "do not exactly match"):
                candidate.finalize_candidate(argparse.Namespace(out_dir=str(root / "candidate"), attestation_bundle=str(bundle_path)))

    def test_reviewed_build_equal_to_a_later_git_floor_is_valid(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["product_version"]["git_build_floor"] = "137"
            write_json(root / "app" / "manifest.json", app_manifest)
            manifest = candidate.build_candidate(args(root))
            self.assertEqual(manifest["version"]["build"], "137")
            self.assertEqual(manifest["dmg"]["name"], "GramDrive-0.5.0-137.dmg")

    def test_candidate_revalidates_build_floor_and_source(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["product_version"]["git_build_floor"] = "138"
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "git-derived floor"):
                candidate.build_candidate(args(root))

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); stage(root)
            app_manifest = candidate.read_json(root / "app" / "manifest.json")
            app_manifest["product_version"]["build_source"] = "git-revision-count"
            write_json(root / "app" / "manifest.json", app_manifest)
            with self.assertRaisesRegex(candidate.CandidateError, "build source"):
                candidate.build_candidate(args(root))

    def test_privacy_scrub_rejects_secret_markers_and_local_paths(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); root.mkdir(exist_ok=True)
            (root / "bad.json").write_text('{"x":"MACOS_CERT_PASSWORD"}')
            with self.assertRaisesRegex(candidate.CandidateError, "privacy scrub"):
                candidate.scrub_text_files(root)


class BuildOrderTests(unittest.TestCase):
    FEED = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item><sparkle:version>123</sparkle:version></item></channel></rss>'

    def stable_config(self, root: str, generation: int = 1, **updates) -> Path:
        config = {
            "schema": 1,
            "active_generation": generation,
            "active_public_key": "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE=",
        }
        config.update(updates)
        path = Path(root) / "sparkle-stable.json"
        path.write_text(json.dumps(config))
        return path

    def stable_manifest(
        self,
        root: str,
        generations: list[int],
        *,
        active_key: str = "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE=",
        feed_payloads: dict[int, bytes] | None = None,
        **updates,
    ) -> Path:
        feed_payloads = feed_payloads or {generation: self.FEED for generation in generations}
        manifest = {
            "schema": 1,
            "archive": {"name": "stable-pages-site.tar.gz", "sha256": "a" * 64, "bytes": 1},
            "files": [
                {
                    "path": f"updates/stable/v{generation}/stable.xml",
                    "sha256": sha256(feed_payloads[generation]).hexdigest(),
                    "bytes": len(feed_payloads[generation]),
                }
                for generation in generations
            ],
            "feed_keys": [
                {"generation": generation, "public_key": active_key}
                for generation in generations
            ],
            "signed_by_generation": max(generations),
        }
        manifest.update(updates)
        path = Path(root) / "stable-pages-site-manifest.json"
        path.write_text(json.dumps(manifest))
        return path

    def test_test_reads_only_test_feed_and_stable_candidate_reads_both(self):
        calls = []
        value, _ = order.highest("test", lambda url: calls.append(url) or self.FEED)
        self.assertEqual(value, 123); self.assertEqual(calls, [order.TEST_FEED])
        with tempfile.TemporaryDirectory() as raw:
            calls.clear()
            stable_feed = self.FEED.replace(b"123", b"124")
            manifest = self.stable_manifest(raw, [1], feed_payloads={1: stable_feed})

            def load(url):
                calls.append(url)
                return stable_feed if "/stable/v1/" in url else self.FEED

            value, _ = order.highest("stable-candidate", load, stable_site_manifest=manifest)
            self.assertEqual(value, 124)
            self.assertEqual(
                calls,
                [order.TEST_FEED, order.STABLE_FEED_TEMPLATE.format(generation=1)],
            )

    def test_missing_initial_feed_means_zero_and_malformed_versions_fail(self):
        self.assertEqual(order.highest("test", lambda _: None)[0], 0)
        with self.assertRaisesRegex(ValueError, "non-canonical"):
            order.builds(self.FEED.replace(b">123<", b">1.2<"))

    def test_legacy_enclosure_version_attribute_is_also_observed(self):
        legacy = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item><enclosure sparkle:version="122"/></item></channel></rss>'
        self.assertEqual(order.builds(legacy), [122])

    def test_present_feed_requires_exact_namespace_and_direct_item_placement(self):
        foreign = self.FEED.replace(
            b"http://www.andymatuschak.org/xml-namespaces/sparkle",
            b"https://example.invalid/not-sparkle",
        )
        missing = b"<rss><channel><item><title>no version</title></item></channel></rss>"
        misplaced_legacy = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item sparkle:version="123"/></channel></rss>'
        channel_version = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><sparkle:version>999</sparkle:version></channel></rss>'
        channel_enclosure = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><enclosure sparkle:version="999"/></channel></rss>'
        nested_version = b'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item><description><sparkle:version>999</sparkle:version></description></item></channel></rss>'
        for payload in (
            foreign, missing, misplaced_legacy, channel_version, channel_enclosure,
            nested_version,
        ):
            with self.subTest(payload=payload):
                with self.assertRaises(ValueError):
                    order.builds(payload)

    def test_sparkle_item_versions_are_byte_canonical_and_unambiguous(self):
        whitespace = self.FEED.replace(b">123<", b"> 123 <")
        duplicate = self.FEED.replace(
            b"</item>", b'<enclosure sparkle:version="123"/></item>',
        )
        duplicate_item = self.FEED.replace(b"</channel>", b"<item><sparkle:version>123</sparkle:version></item></channel>")
        zero = self.FEED.replace(b">123<", b">0<")
        for payload in (whitespace, duplicate, duplicate_item, zero):
            with self.subTest(payload=payload):
                with self.assertRaises(ValueError):
                    order.builds(payload)

    def test_sparkle_version_element_is_an_attribute_free_child_free_leaf(self):
        attributed = self.FEED.replace(b"<sparkle:version>", b'<sparkle:version format="decimal">')
        nested = self.FEED.replace(b"123</sparkle:version>", b"123<ignored/></sparkle:version>")
        for payload in (attributed, nested):
            with self.subTest(payload=payload):
                with self.assertRaisesRegex(ValueError, "attribute-free leaf"):
                    order.builds(payload)
        formatted = self.FEED.replace(b"</sparkle:version></item>", b"</sparkle:version>\n</item>")
        self.assertEqual(order.builds(formatted), [123])

    def test_malformed_xml_version_and_network_failure_fail_closed(self):
        for payload in (b"<rss>", self.FEED.replace(b">123<", b">0<")):
            with self.subTest(payload=payload):
                with self.assertRaises((ValueError, order.ET.ParseError)):
                    order.builds(payload)
        with self.assertRaises(order.urllib.error.URLError):
            order.highest("test", lambda _: (_ for _ in ()).throw(order.urllib.error.URLError("offline")))

    def test_rotated_stable_endpoint_union_retains_prior_floor(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw, generation=2)
            stable_feed = self.FEED.replace(b"123", b"124")
            manifest = self.stable_manifest(raw, [1], feed_payloads={1: stable_feed})
            calls = []

            def load(url):
                calls.append(url)
                if "/stable/v2/" in url:
                    return None
                return stable_feed if "/stable/v1/" in url else self.FEED

            value, present = order.highest(
                "stable-candidate", load, stable_config=config,
                stable_site_manifest=manifest,
            )
            self.assertEqual(value, 124)
            self.assertEqual(len(present), 2)
            self.assertEqual(
                calls,
                [
                    order.TEST_FEED,
                    order.STABLE_FEED_TEMPLATE.format(generation=1),
                    order.STABLE_FEED_TEMPLATE.format(generation=2),
                ],
            )

    def test_no_prior_manifest_allows_only_initial_v1_absence(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw)
            self.assertEqual(
                order.highest("stable-candidate", lambda _: None, stable_config=config)[0],
                0,
            )
            rotated = self.stable_config(raw, generation=2)
            with self.assertRaisesRegex(ValueError, "authenticated prior site"):
                order.highest("stable-candidate", lambda _: None, stable_config=rotated)

    def test_created_v1_disappearance_fails_closed(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw)
            manifest = self.stable_manifest(raw, [1])
            with self.assertRaisesRegex(ValueError, "required prior stable feed is missing"):
                order.highest(
                    "stable-candidate", lambda _: None, stable_config=config,
                    stable_site_manifest=manifest,
                )

    def test_created_v2_disappearance_fails_closed(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw, generation=2)
            manifest = self.stable_manifest(raw, [1, 2])
            with self.assertRaisesRegex(ValueError, "required prior stable feed is missing"):
                order.highest(
                    "stable-candidate", lambda url: None if "/stable/v2/" in url else self.FEED,
                    stable_config=config,
                    stable_site_manifest=manifest,
                )

    def test_malformed_or_mismatched_authenticated_manifest_fails_closed(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw, generation=2)
            cases = (
                {"feed_keys": []},
                {"feed_keys": [{"generation": 2, "public_key": "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE="}]},
                {"signed_by_generation": 2},
                {"files": []},
            )
            for updates in cases:
                with self.subTest(updates=updates):
                    manifest = self.stable_manifest(raw, [1], **updates)
                    with self.assertRaises(ValueError):
                        order.feed_endpoints("stable-candidate", config, manifest)
            wrong_key = base64.b64encode(b"x" * 32).decode()
            manifest = self.stable_manifest(raw, [1, 2], active_key=wrong_key)
            with self.assertRaisesRegex(ValueError, "does not match reviewed config"):
                order.feed_endpoints("stable-candidate", config, manifest)

    def test_authenticated_manifest_records_are_exact_safe_unique_and_ordered(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw)
            valid = json.loads(self.stable_manifest(raw, [1]).read_text())
            cases = []
            for archive in (
                None,
                {"name": "wrong.tar.gz", "sha256": "a" * 64, "bytes": 1},
                {"name": "stable-pages-site.tar.gz", "sha256": "A" * 64, "bytes": 1},
                {"name": "stable-pages-site.tar.gz", "sha256": "a" * 64, "bytes": 0},
                {"name": "stable-pages-site.tar.gz", "sha256": "a" * 64, "bytes": -1},
            ):
                changed = dict(valid); changed["archive"] = archive; cases.append(changed)
            for files in (
                [{"path": "updates/stable/v1/stable.xml"}],
                [
                    valid["files"][0],
                    {"path": "updates/stable/v1/stable.xml", "sha256": "c" * 64, "bytes": 2},
                ],
                [{"path": "../stable.xml", "sha256": "c" * 64, "bytes": 2}],
                [
                    {"path": "z.txt", "sha256": "c" * 64, "bytes": 2},
                    valid["files"][0],
                ],
                [{"path": "updates/stable/v01/stable.xml", "sha256": "c" * 64, "bytes": 2}],
            ):
                changed = dict(valid); changed["files"] = files; cases.append(changed)
            for index, value in enumerate(cases):
                with self.subTest(index=index):
                    path = Path(raw) / f"invalid-{index}.json"
                    path.write_text(json.dumps(value))
                    with self.assertRaises(ValueError):
                        order.feed_endpoints("stable-candidate", config, path)

    def test_stable_feed_bytes_must_match_authenticated_manifest(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw)
            manifest = self.stable_manifest(raw, [1])
            changed = self.FEED.replace(b"123", b"124")
            with self.assertRaisesRegex(ValueError, "digest changed"):
                order.highest(
                    "stable-candidate",
                    lambda url: changed if "/stable/v1/" in url else self.FEED,
                    stable_config=config,
                    stable_site_manifest=manifest,
                )
            with self.assertRaisesRegex(ValueError, "byte count changed"):
                order.highest(
                    "stable-candidate",
                    lambda url: self.FEED + b"\n" if "/stable/v1/" in url else self.FEED,
                    stable_config=config,
                    stable_site_manifest=manifest,
                )

    def test_unrecorded_active_stable_endpoint_must_be_404(self):
        with tempfile.TemporaryDirectory() as raw:
            config = self.stable_config(raw)
            with self.assertRaisesRegex(ValueError, "unauthenticated bytes"):
                order.highest("stable-candidate", lambda _: self.FEED, stable_config=config)

    def test_invalid_stable_configuration_fails_closed(self):
        invalid = (
            {"active_generation": 0},
            {"schema": 2},
            {"active_public_key": "not-base64"},
            {"unexpected": True},
        )
        with tempfile.TemporaryDirectory() as raw:
            for updates in invalid:
                with self.subTest(updates=updates):
                    path = self.stable_config(raw, **updates)
                    with self.assertRaises(ValueError):
                        order.feed_endpoints("stable-candidate", path)

    def test_existing_same_commit_test_build_advances_stable_candidate(self):
        self.assertEqual(order.select_build(git_build=114, published_highest=114), 115)

    def test_later_git_and_published_builds_remain_monotonic(self):
        self.assertEqual(order.select_build(git_build=116, published_highest=115), 116)
        self.assertEqual(order.select_build(git_build=116, published_highest=119), 120)

    def test_final_handoff_revalidation_rejects_concurrent_collision(self):
        order.validate_selected_build(candidate_build=115, published_highest=114)
        with self.assertRaisesRegex(ValueError, "no longer newer"):
            order.validate_selected_build(candidate_build=115, published_highest=115)


class StableReleaseSelectionTests(unittest.TestCase):
    @staticmethod
    def release(
        tag,
        assets=(),
        *,
        draft=False,
        prerelease=False,
        published=True,
        published_at=None,
    ):
        return {
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "published_at": (
                published_at
                if published_at is not None
                else ("2026-08-19T00:00:00Z" if published else None)
            ),
            "assets": [{"name": name} for name in assets],
        }

    @staticmethod
    def complete_prior(tag):
        return StableReleaseSelectionTests.release(tag, release_selection.PRIOR_SITE_ASSETS)

    @classmethod
    def filler_page(cls, count=100):
        return [cls.release(f"nonstable-{index}") for index in range(count)]

    def test_candidate_uses_latest_semver_state_head_and_requires_singular_evidence(self):
        older = self.release("v1.9.9", release_selection.CANDIDATE_ASSETS)
        newer = self.release("v1.10.0", release_selection.CANDIDATE_ASSETS)
        self.assertEqual(
            release_selection.select_candidate_state_head([newer, older]), "v1.10.0"
        )
        incomplete = self.release("v2.0.0", (release_selection.CANDIDATE_ASSETS[0],))
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "exactly one"):
            release_selection.select_candidate_state_head([older, incomplete])
        duplicated = self.release(
            "v2.0.0",
            release_selection.CANDIDATE_ASSETS + (release_selection.CANDIDATE_ASSETS[0],),
        )
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "found 2"):
            release_selection.select_candidate_state_head([duplicated])

    def test_stable_prior_fixtures_resume_absent_partial_or_complete_current(self):
        prior = self.complete_prior("v0.1.1")
        current_states = (
            [],
            [self.release("v0.1.2", ("GramDrive-0.1.2-115.dmg",))],
            [self.complete_prior("v0.1.2")],
        )
        for current in current_states:
            with self.subTest(current=current):
                self.assertEqual(
                    release_selection.select_prior_site([prior, *current], "v0.1.2"),
                    "v0.1.1",
                )

    def test_stable_prior_supports_first_release_and_refuses_incomplete_latest_prior(self):
        self.assertIsNone(release_selection.select_prior_site([], "v0.1.2"))
        self.assertIsNone(
            release_selection.select_prior_site(
                [self.complete_prior("v0.1.2")], "v0.1.2"
            )
        )
        complete_old = self.complete_prior("v0.1.0")
        incomplete_latest = self.release("v0.1.1", release_selection.PRIOR_SITE_ASSETS[:-1])
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "v0.1.1"):
            release_selection.select_prior_site(
                [complete_old, incomplete_latest], "v0.1.2"
            )

    def test_stable_prior_refuses_rerunning_older_tag_beneath_newer_state(self):
        newer = self.release("v0.1.3", release_selection.CANDIDATE_ASSETS)
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "newer published"):
            release_selection.select_prior_site([newer], "v0.1.2")

    def test_paginated_candidate_considers_complete_state_beyond_record_100(self):
        pages = [
            self.filler_page(),
            [self.release("v9.0.0", release_selection.CANDIDATE_ASSETS)],
        ]
        flattened = release_selection.flatten_release_pages(pages)
        self.assertEqual(release_selection.select_candidate_state_head(flattened), "v9.0.0")
        pages[1][0] = self.release("v9.0.0", (release_selection.CANDIDATE_ASSETS[0],))
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "v9.0.0"):
            release_selection.select_candidate_state_head(
                release_selection.flatten_release_pages(pages)
            )

    def test_paginated_stable_prior_considers_newer_and_incomplete_state_beyond_100(self):
        page_one = [self.complete_prior("v0.1.0"), *self.filler_page(99)]
        newer_pages = [page_one, [self.release("v0.1.3", release_selection.CANDIDATE_ASSETS)]]
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "newer published"):
            release_selection.select_prior_site(
                release_selection.flatten_release_pages(newer_pages), "v0.1.2"
            )
        incomplete_pages = [
            page_one,
            [self.release("v0.1.1", release_selection.PRIOR_SITE_ASSETS[:-1])],
        ]
        with self.assertRaisesRegex(release_selection.ReleaseSelectionError, "v0.1.1"):
            release_selection.select_prior_site(
                release_selection.flatten_release_pages(incomplete_pages), "v0.1.2"
            )

    def test_paginated_payload_shape_fails_closed(self):
        invalid = (None, {}, [], [None], [{"not": "a page"}], [[self.release("v1.0.0")], {}])
        for payload in invalid:
            with self.subTest(payload=payload):
                with self.assertRaises(release_selection.ReleaseSelectionError):
                    release_selection.flatten_release_pages(payload)
        self.assertEqual(release_selection.flatten_release_pages([[]]), [])

    def test_published_stable_timestamp_is_strict_timezone_aware_rfc3339(self):
        valid = (
            "2026-08-19T00:00:00Z",
            "2026-08-19T00:00:00.123456Z",
            "2026-08-19T04:00:00+04:00",
        )
        for timestamp in valid:
            with self.subTest(timestamp=timestamp):
                release = self.release(
                    "v1.0.0", release_selection.CANDIDATE_ASSETS, published_at=timestamp
                )
                self.assertEqual(release_selection.select_candidate_state_head([release]), "v1.0.0")
        invalid = (
            None,
            "",
            " ",
            123,
            "2026-08-19",
            "2026-08-19T00:00:00",
            "2026-02-30T00:00:00Z",
            "2026-08-19T00:00:00Z trailing",
        )
        for timestamp in invalid:
            with self.subTest(timestamp=timestamp):
                release = self.release(
                    "v1.0.0", release_selection.CANDIDATE_ASSETS, published_at=timestamp
                )
                if timestamp is None:
                    release["published_at"] = None
                with self.assertRaisesRegex(
                    release_selection.ReleaseSelectionError, "invalid publication time"
                ):
                    release_selection.select_candidate_state_head([release])

    def test_malformed_release_records_fail_closed(self):
        invalid = (
            None,
            [],
            {},
            {"tag_name": "v1.0.0", "draft": "false", "prerelease": False},
            {"tag_name": 1, "draft": False, "prerelease": False},
            {
                "tag_name": "nonstable",
                "draft": False,
                "prerelease": False,
                "published_at": "2026-08-19T00:00:00Z",
                "assets": None,
            },
            {
                "tag_name": "updates-test-v1",
                "draft": False,
                "prerelease": True,
                "published_at": " ",
                "assets": [],
            },
        )
        for record in invalid:
            with self.subTest(record=record):
                with self.assertRaises(release_selection.ReleaseSelectionError):
                    release_selection.published_stable_releases([record])

    def test_selector_cli_consumes_paginated_api_fixture_without_jq(self):
        with tempfile.TemporaryDirectory() as raw:
            fixture = Path(raw) / "release-pages.json"
            fixture.write_text(json.dumps([[self.complete_prior("v0.1.1")]]))
            command = [
                sys.executable,
                str(REPO / ".scripts/release/select_stable_release.py"),
                "--mode", "stable-prior",
                "--current-tag", "v0.1.2",
                "--release-pages", str(fixture),
            ]
            result = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "v0.1.1\n")

    def test_selector_cli_rejects_malformed_input_and_mode_misuse(self):
        with tempfile.TemporaryDirectory() as raw:
            fixture = Path(raw) / "release-pages.json"
            base = [
                sys.executable,
                str(REPO / ".scripts/release/select_stable_release.py"),
                "--release-pages", str(fixture),
            ]
            invalid_payloads = (
                "not json",
                json.dumps([{}]),
                json.dumps([[None]]),
                json.dumps(
                    [[self.release("v1.0.0", release_selection.CANDIDATE_ASSETS, published_at=" ")]]
                ),
            )
            for payload in invalid_payloads:
                with self.subTest(payload=payload):
                    fixture.write_text(payload)
                    malformed = subprocess.run(
                        [*base, "--mode", "candidate-state-head"],
                        check=False,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(malformed.returncode, 1)
                    self.assertIn("SELECTION FAILED", malformed.stderr)
            fixture.write_text(json.dumps([[]]))
            missing_tag = subprocess.run(
                [*base, "--mode", "stable-prior"], check=False, capture_output=True, text=True
            )
            self.assertNotEqual(missing_tag.returncode, 0)
            extra_tag = subprocess.run(
                [*base, "--mode", "candidate-state-head", "--current-tag", "v1.0.0"],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(extra_tag.returncode, 0)
            invalid_mode = subprocess.run(
                [*base, "--mode", "unknown"], check=False, capture_output=True, text=True
            )
            self.assertNotEqual(invalid_mode.returncode, 0)


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = (REPO / ".github/workflows/candidate-build.yml").read_text()
        cls.stable_workflow = (REPO / ".github/workflows/release.yml").read_text()
        cls.candidate_job = cls.text[
            cls.text.index("  candidate:"):cls.text.index("  publish-test:")
        ]

    def test_trigger_environment_permissions_and_dedicated_concurrency(self):
        self.assertIn("branches: [main]", self.text)
        self.assertIn("workflow_dispatch:", self.text)
        self.assertIn('if [ "$GITHUB_REF" != "refs/heads/main" ]', self.text)
        self.assertIn("stable-candidate", self.text)
        self.assertIn("environment: updates-test", self.text)
        self.assertIn("runs-on: [self-hosted, gramdrive-mac]", self.text)
        self.assertIn("group: gramdrive-sparkle-publication-handoff", self.text)
        self.assertIn("group: gramdrive-sparkle-publication-handoff", self.stable_workflow)
        self.assertIn("group: gramdrive-candidate-signing-runner", self.text)
        self.assertIn("cancel-in-progress: false", self.text)
        self.assertIn("contents: read", self.text)
        self.assertIn("id-token: write", self.text)
        self.assertIn("attestations: write", self.text)
        self.assertNotIn("contents: write", self.candidate_job)
        self.assertNotIn("pages: write", self.candidate_job)

    def test_workflow_selects_one_build_and_revalidates_it_at_handoff(self):
        self.assertIn('--git-build "$(git rev-list --count HEAD)"', self.candidate_job)
        self.assertIn('--build-number "${{ steps.build_order.outputs.build_number }}"', self.candidate_job)
        self.assertIn('--candidate-build "${{ steps.build_order.outputs.build_number }}"', self.candidate_job)
        self.assertEqual(self.candidate_job.count("--stable-site-manifest"), 2)
        finalize = self.candidate_job.index("name: Bind and reverify the attestation bundle")
        revalidate = self.candidate_job.index(
            "name: Recheck monotonic ordering at the immutable handoff boundary"
        )
        handoff = self.candidate_job.index("name: Name the immutable handoff")
        upload = self.candidate_job.index("name: Upload one immutable verified candidate")
        self.assertLess(finalize, revalidate)
        self.assertLess(revalidate, handoff)
        self.assertLess(handoff, upload)

    def test_stable_creation_state_is_authenticated_before_selection(self):
        restore = self.candidate_job.index("name: Restore authenticated stable feed creation state")
        verify = self.candidate_job.index("gh attestation verify", restore)
        selection = self.candidate_job.index("name: Check monotonic Sparkle build ordering")
        self.assertLess(restore, verify)
        self.assertLess(verify, selection)
        state = self.candidate_job[restore:selection]
        self.assertIn("stable-pages-site-manifest.json", state)
        self.assertIn("stable-pages-site.attestation.json", state)
        self.assertIn(".github/workflows/release.yml", state)
        self.assertIn('--source-digest "$previous_commit"', state)
        self.assertIn("contents: read", self.candidate_job)
        self.assertNotIn("contents: write", self.candidate_job)
        self.assertNotIn("pages: write", self.candidate_job)

    def test_candidate_and_stable_use_distinct_checked_in_release_selection_modes(self):
        helper = ".scripts/release/select_stable_release.py"
        self.assertIn(helper, self.candidate_job)
        self.assertIn("--mode candidate-state-head", self.candidate_job)
        self.assertNotIn("--current-tag", self.candidate_job)
        self.assertIn(helper, self.stable_workflow)
        self.assertIn("--mode stable-prior", self.stable_workflow)
        self.assertIn('--current-tag "${{ steps.source.outputs.tag }}"', self.stable_workflow)
        self.assertNotIn("$(jq ", self.candidate_job)
        self.assertNotIn("$(jq ", self.stable_workflow)
        for workflow in (self.candidate_job, self.stable_workflow):
            self.assertIn("gh api --paginate --slurp", workflow)
            self.assertIn('"repos/$GITHUB_REPOSITORY/releases?per_page=100"', workflow)
            self.assertIn("--release-pages", workflow)
            self.assertNotIn("--releases ", workflow)
            self.assertIn("stable-pages-site-manifest.json", workflow)
            self.assertIn("stable-pages-site.attestation.json", workflow)

    def test_paginated_discovery_fails_before_any_release_or_pages_mutation(self):
        for workflow in (self.candidate_job, self.stable_workflow):
            with self.subTest(workflow=workflow[:40]):
                pagination = workflow.index("gh api --paginate --slurp")
                selection = workflow.index(".scripts/release/select_stable_release.py", pagination)
                self.assertLess(pagination, selection)
                step_start = workflow.rfind("run: |", 0, pagination)
                self.assertIn("set -euo pipefail", workflow[step_start:pagination])
                mutations = [
                    position
                    for token in (
                        "gh release create",
                        "gh release upload",
                        "actions/upload-pages-artifact",
                        "actions/deploy-pages",
                    )
                    if (position := workflow.find(token)) >= 0
                ]
                if mutations:
                    self.assertLess(selection, min(mutations))

    def test_job_has_only_apple_secrets_and_no_publication_capability(self):
        for name in ("MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "APPSTORE_KEY_ID", "APPSTORE_ISSUER_ID", "APPSTORE_PRIVATE_KEY"):
            self.assertIn(f"secrets.{name}", self.candidate_job)
        for forbidden in (
            "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64",
            "SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64",
            "SPARKLE_STABLE_EDDSA_PRIVATE_KEY_B64",
            "SPARKLE_STABLE_PREVIOUS_EDDSA_PRIVATE_KEY_B64",
            "gh release create",
            "gh release upload",
            "gh release delete",
            "test.xml#",
            "deploy-pages",
            "upload-pages-artifact",
        ):
            self.assertNotIn(forbidden, self.candidate_job)

    def test_live_candidate_uses_the_locked_sparkle_dependency(self):
        resolved = json.loads((REPO / "apple/GramDriveSupport/Package.resolved").read_text())
        sparkle = next(pin for pin in resolved["pins"] if pin["identity"] == "sparkle")
        self.assertEqual(sparkle["state"]["version"], "2.9.5")
        self.assertEqual(sparkle["state"]["revision"], "79bc9e872948e47877e76f194cb0c8e0412b0b90")

    def test_upload_is_after_attestation_verification_and_cleanup_is_always(self):
        attest = self.text.index("name: Attest the exact candidate subjects")
        verify = self.text.index("name: Bind and reverify the attestation bundle")
        upload = self.text.index("name: Upload one immutable verified candidate")
        cleanup = self.text.index("name: Remove all credential and workspace state")
        self.assertLess(attest, verify); self.assertLess(verify, upload); self.assertLess(upload, cleanup)
        self.assertIn("if-no-files-found: error", self.text[upload:cleanup])
        self.assertIn("overwrite: false", self.text[upload:cleanup])
        self.assertIn("if: always()", self.text[cleanup:])
        self.assertIn('security import "$cert_path" -k "$KEYCHAIN_PATH" -P "$MACOS_CERT_PASSWORD"', self.text)
        self.assertNotIn("set -x", self.text)

    def test_x86_signing_host_restores_and_cross_links_arm64_tdlib_before_credentials(self):
        restore_step = self.text.index("name: Restore and prove the pinned arm64 live TDLib core")
        credentials = self.text.index("name: Import Developer ID")
        self.assertLess(restore_step, credentials)
        section = self.text[restore_step:credentials]
        self.assertIn('test "$(uname -m)" = "x86_64"', section)
        self.assertIn("restore_pinned_artifact.py", section)
        self.assertIn("make tdlib-smoke-link", section)
        self.assertIn("GRAMDRIVE_TDLIB_ARTIFACT_DIR=.temp/tdlib/out make package", section)
        self.assertNotIn("make tdlib\n", section)
        self.assertNotIn("make tdjson-smoke", section)


if __name__ == "__main__":
    unittest.main()
