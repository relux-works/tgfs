#!/usr/bin/env python3
"""Regression tests for the candidate signing/notarization trust boundary."""
from __future__ import annotations

import argparse
import base64
import importlib.util
import json
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
COMMIT = "a" * 40
DMG = b"exact candidate dmg bytes"
DMG_SHA = sha256(DMG).hexdigest()
TDLIB_SHA = sha256(b"tdlib").hexdigest()
CORE_SHA = sha256(b"core").hexdigest()
CORE_ZIP_SHA = sha256(b"core zip").hexdigest()


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
    embedded_tdlib.write_bytes(b"tdlib")
    app_checksums = {
        "GramDrive-0.5.0.dmg": DMG_SHA,
        "GramDrive.app/Contents/Frameworks/libtdjson.dylib": TDLIB_SHA,
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
        "tdjson": {"linked": True, "embedded_libraries": ["libtdjson.dylib"]},
        "product_version": {"short": "0.5.0", "build": "137"},
        "checksums": app_checksums, "sizes": {"dmg_bytes": len(DMG)},
        "toolchain": {"swift": "Swift test"},
    }
    core_artifact = core_dir / "GramDriveCore"
    core_artifact.mkdir()
    (core_artifact / "core.bin").write_bytes(b"core")
    core_zip = core_dir / "GramDriveCore-0.5.0.zip"
    core_zip.write_bytes(b"core zip")
    core_checksums = {"core.bin": CORE_SHA, "../GramDriveCore-0.5.0.zip": CORE_ZIP_SHA}
    core = {
        "git": {"commit": COMMIT, "worktree_clean": True}, "tdjson": {"linked": True, "library_sha256": TDLIB_SHA},
        "host_test_slice": None, "toolchain": {"rustc": "rustc test"}, "checksums": core_checksums,
    }
    (tdlib_dir / "lib").mkdir()
    (tdlib_dir / "lib" / "libtdjson.dylib").write_bytes(b"tdlib")
    tdlib_checksums = {"lib/libtdjson.dylib": TDLIB_SHA}
    tdlib = {
        "gramdrive": {"commit": COMMIT, "worktree_clean": True},
        "tdlib": {"repo": "https://github.com/tdlib/td.git", "commit": "b" * 40, "runtime_version": "1.8.51"},
        "target": {"label": "macos-arm64", "arch": "arm64"},
        "reproducibility": {"clean_build_tree": True},
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
            with self.assertRaisesRegex(candidate.CandidateError, "embedded live TDLib bytes"):
                candidate.build_candidate(args(second))

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

    def test_privacy_scrub_rejects_secret_markers_and_local_paths(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw); root.mkdir(exist_ok=True)
            (root / "bad.json").write_text('{"x":"MACOS_CERT_PASSWORD"}')
            with self.assertRaisesRegex(candidate.CandidateError, "privacy scrub"):
                candidate.scrub_text_files(root)


class BuildOrderTests(unittest.TestCase):
    FEED = b'<rss xmlns:sparkle="https://sparkle-project.org/xml-namespaces/sparkle"><channel><item><enclosure sparkle:version="123"/></item></channel></rss>'

    def test_test_reads_only_test_feed_and_stable_candidate_reads_both(self):
        calls = []
        value, _ = order.highest("test", lambda url: calls.append(url) or self.FEED)
        self.assertEqual(value, 123); self.assertEqual(calls, [order.TEST_FEED])
        calls.clear()
        def load(url):
            calls.append(url)
            return self.FEED.replace(b"123", b"124") if url == order.STABLE_FEED else self.FEED
        value, _ = order.highest("stable-candidate", load)
        self.assertEqual(value, 124); self.assertEqual(calls, [order.TEST_FEED, order.STABLE_FEED])

    def test_missing_initial_feed_means_zero_and_malformed_versions_fail(self):
        self.assertEqual(order.highest("test", lambda _: None)[0], 0)
        with self.assertRaisesRegex(ValueError, "non-numeric"):
            order.builds(self.FEED.replace(b'123', b'1.2'))


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = (REPO / ".github/workflows/candidate-build.yml").read_text()

    def test_trigger_environment_permissions_and_dedicated_concurrency(self):
        self.assertIn("branches: [main]", self.text)
        self.assertIn("workflow_dispatch:", self.text)
        self.assertIn('if [ "$GITHUB_REF" != "refs/heads/main" ]', self.text)
        self.assertIn("stable-candidate", self.text)
        self.assertIn("environment: updates-test", self.text)
        self.assertIn("runs-on: [self-hosted, gramdrive-mac]", self.text)
        self.assertIn("group: gramdrive-candidate-signing-runner", self.text)
        self.assertIn("cancel-in-progress: false", self.text)
        self.assertIn("contents: read", self.text)
        self.assertIn("id-token: write", self.text)
        self.assertIn("attestations: write", self.text)
        self.assertNotIn("contents: write", self.text)
        self.assertNotIn("pages: write", self.text)

    def test_job_has_only_apple_secrets_and_no_publication_capability(self):
        for name in ("MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "APPSTORE_KEY_ID", "APPSTORE_ISSUER_ID", "APPSTORE_PRIVATE_KEY"):
            self.assertIn(f"secrets.{name}", self.text)
        for forbidden in ("SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64", "SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64", "gh release", "test.xml#", "deploy-pages", "upload-pages-artifact"):
            self.assertNotIn(forbidden, self.text)

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


if __name__ == "__main__":
    unittest.main()
