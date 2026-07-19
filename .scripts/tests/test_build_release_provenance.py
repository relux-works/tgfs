#!/usr/bin/env python3
"""Tests for .scripts/release/build_release_provenance.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests
     (or via the gate: run_automated.py --suite repo --run-id local-repo)

No test shells out to cargo or git: a fake runner stands in for both, so the
suite is fast and runs anywhere. What that buys is coverage of the properties a
reader of the release cannot check for themselves — that the SBOM inventories
every third-party crate with its license, that the changelog spans exactly the
commits this tag introduces, that the release manifest ties every artifact to a
sha256, and that the credential scrub catches a planted secret before a release
could ship one.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from hashlib import sha256
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / ".scripts" / "release" / "build_release_provenance.py"


def load_module():
    spec = importlib.util.spec_from_file_location("build_release_provenance", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


rel = load_module()


# A small resolved Cargo graph: two third-party crates (one with an SPDX
# expression, one dual license-file only), plus a workspace crate with no source
# (the product itself), which must NOT appear as a dependency component.
CARGO_METADATA = json.dumps(
    {
        "packages": [
            {
                "name": "zzz-last",
                "version": "0.2.0",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "license": "MIT OR Apache-2.0",
            },
            {
                "name": "aaa-first",
                "version": "1.0.0",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "license": None,
                "license_file": "LICENSE",
            },
            {
                # Our own workspace crate — no `source`. Not a dependency.
                "name": "gramdrive-core",
                "version": "0.5.0",
                "source": None,
                "license": None,
            },
        ]
    }
)


def app_manifest(**overrides) -> dict:
    base = {
        "schema": 1,
        "name": "GramDrive",
        "product_version": {"short": "1.2.0", "build": "137"},
        "platform": "macos-arm64",
        "signing_identity": "Developer ID Application: Relux Works, LLC (262RZ595FP)",
        "team_id": "262RZ595FP",
        "core_contract_version": "0.5.0",
        "git": {"describe": "v1.2.0", "commit": "abc123def456", "worktree_clean": True},
        "toolchain": {"swift": "Apple Swift version 6.3", "rustc": "rustc 1.91.0"},
        "notarization": {"submitted": True, "id": "sub-xyz", "status": "Accepted"},
        "source_date": "2026-07-19T00:00:00+00:00",
    }
    base.update(overrides)
    return base


class FakeGit:
    """A tiny scripted git+cargo runner keyed by command prefix."""

    def __init__(self, *, prev_tag="v1.1.0", commits=("aaa fix things (Dev)", "bbb add feature (Dev)")):
        self.prev_tag = prev_tag
        self.commits = commits

    def __call__(self, argv, cwd):
        argv = [str(a) for a in argv]
        joined = " ".join(argv)
        if joined.startswith("cargo metadata"):
            return 0, CARGO_METADATA
        if "rev-parse --verify --quiet" in joined:
            ref = argv[-1]
            if ref.startswith("v1.2.0"):
                return 0, "abc123def456\n"
            if self.prev_tag and ref.startswith(self.prev_tag):
                return 0, "prev000commit\n"
            return 1, ""
        if "describe --tags --abbrev=0" in joined:
            return (0, f"{self.prev_tag}\n") if self.prev_tag else (1, "")
        if joined.startswith("git log"):
            return 0, "\n".join(self.commits) + ("\n" if self.commits else "")
        return 0, ""


def stage_package(tmp: Path, *, manifest=None, dmg="GramDrive-1.2.0.dmg") -> Path:
    pkg = tmp / "app-packaging"
    pkg.mkdir(parents=True)
    manifest = manifest if manifest is not None else app_manifest()
    (pkg / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    if dmg:
        (pkg / dmg).write_bytes(b"dmg-bytes")
        digest = sha256(b"dmg-bytes").hexdigest()
        (pkg / "CHECKSUMS.sha256").write_text(
            f"{digest}  {dmg}\n" f"{'0' * 64}  GramDrive.app/Contents/Info.plist\n", encoding="utf-8"
        )
    return pkg


def run(tmp: Path, *, tag="v1.2.0", runner=None, manifest=None, dmg="GramDrive-1.2.0.dmg"):
    pkg = stage_package(tmp, manifest=manifest, dmg=dmg)
    out = tmp / "release"
    m = rel.generate(
        tmp,
        package_dir=pkg,
        out_dir=out,
        tag=tag,
        runner=runner or FakeGit(),
        echo=lambda _: None,
        environ={},
    )
    return m, out


class SbomTest(unittest.TestCase):
    def test_only_third_party_crates_are_components_and_sorted(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            sbom = json.loads((out / "sbom.json").read_text())
            names = [c["name"] for c in sbom["components"]]
            # The workspace crate (no source) is excluded; the rest are sorted.
            self.assertEqual(names, ["aaa-first", "zzz-last"])
            self.assertEqual(sbom["bomFormat"], "CycloneDX")
            self.assertEqual(sbom["specVersion"], "1.5")

    def test_purls_and_license_expression(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            sbom = json.loads((out / "sbom.json").read_text())
            by_name = {c["name"]: c for c in sbom["components"]}
            self.assertEqual(by_name["zzz-last"]["purl"], "pkg:cargo/zzz-last@0.2.0")
            self.assertEqual(
                by_name["zzz-last"]["licenses"], [{"expression": "MIT OR Apache-2.0"}]
            )

    def test_missing_license_is_flagged_not_faked(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            sbom = json.loads((out / "sbom.json").read_text())
            by_name = {c["name"]: c for c in sbom["components"]}
            self.assertEqual(by_name["aaa-first"]["licenses"], [])
            notes = [p["value"] for p in by_name["aaa-first"]["properties"]]
            self.assertTrue(any("license_file=LICENSE" in n for n in notes))

    def test_serial_is_deterministic_for_the_same_commit_and_tag(self):
        self.assertEqual(
            rel.serial_number("abc", "v1"), rel.serial_number("abc", "v1")
        )
        self.assertNotEqual(
            rel.serial_number("abc", "v1"), rel.serial_number("abc", "v2")
        )

    def test_metadata_points_at_the_license_gate_not_a_second_policy(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            sbom = json.loads((out / "sbom.json").read_text())
            props = {p["name"]: p["value"] for p in sbom["metadata"]["properties"]}
            self.assertIn("cargo deny check", props["gramdrive:license-gate"])
            self.assertEqual(props["gramdrive:cargo-components"], "2")
            self.assertEqual(props["gramdrive:swiftpm-components"], "0")


class ChangelogTest(unittest.TestCase):
    def test_changelog_spans_since_the_previous_tag(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            text = (out / "CHANGELOG.md").read_text()
            self.assertIn("# GramDrive 1.2.0 (v1.2.0)", text)
            self.assertIn("Changes since v1.1.0", text)
            self.assertIn("- aaa fix things (Dev)", text)
            self.assertIn("- bbb add feature (Dev)", text)

    def test_first_release_uses_full_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp), runner=FakeGit(prev_tag=None))
            text = (out / "CHANGELOG.md").read_text()
            self.assertIn("First tagged release", text)


class RollbackTest(unittest.TestCase):
    def test_rollback_names_the_previous_release_and_the_dmg(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            rb = json.loads((out / "rollback.json").read_text())
            self.assertEqual(rb["tag"], "v1.2.0")
            self.assertEqual(rb["commit"], "abc123def456")
            self.assertEqual(rb["previous"]["tag"], "v1.1.0")
            self.assertEqual(rb["previous"]["commit"], "prev000commit")
            self.assertEqual(rb["artifact"]["dmg"], "GramDrive-1.2.0.dmg")
            self.assertTrue(rb["artifact"]["notarized"])
            self.assertIn("v1.1.0", rb["rollback"])

    def test_first_release_rollback_says_so(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp), runner=FakeGit(prev_tag=None))
            rb = json.loads((out / "rollback.json").read_text())
            self.assertIsNone(rb["previous"]["tag"])
            self.assertIn("First release", rb["rollback"])


class ReleaseManifestTest(unittest.TestCase):
    def test_manifest_ties_every_artifact_to_a_sha256(self):
        with tempfile.TemporaryDirectory() as tmp:
            m, out = run(Path(tmp))
            self.assertEqual(m["tag"], "v1.2.0")
            self.assertEqual(m["commit"], "abc123def456")
            self.assertEqual(m["version"], {"short": "1.2.0", "build": "137"})
            names = {a["name"] for a in m["artifacts"]}
            self.assertIn("GramDrive-1.2.0.dmg", names)
            self.assertIn("sbom.json", names)
            self.assertIn("CHANGELOG.md", names)
            self.assertIn("rollback.json", names)
            # The recorded sbom digest actually matches the file on disk.
            digests = {a["name"]: a["sha256"] for a in m["artifacts"]}
            self.assertEqual(digests["sbom.json"], rel.sha256_file(out / "sbom.json"))
            self.assertEqual(
                digests["GramDrive-1.2.0.dmg"], sha256(b"dmg-bytes").hexdigest()
            )

    def test_manifest_carries_notarization_and_toolchain_from_the_app(self):
        with tempfile.TemporaryDirectory() as tmp:
            m, _ = run(Path(tmp))
            self.assertTrue(m["notarization"]["submitted"])
            self.assertEqual(m["toolchain"]["rustc"], "rustc 1.91.0")
            self.assertEqual(m["team_id"], "262RZ595FP")
            self.assertEqual(m["credential_scrub"], "passed")

    def test_release_checksums_file_covers_every_provenance_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, out = run(Path(tmp))
            rendered = (out / "RELEASE-CHECKSUMS.sha256").read_text()
            for name in ("sbom.json", "CHANGELOG.md", "rollback.json", "release-manifest.json"):
                self.assertIn(name, rendered)


class VersionSourceTest(unittest.TestCase):
    def test_version_and_commit_come_from_the_app_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = app_manifest(product_version={"short": "2.5.1", "build": "999"})
            m, _ = run(Path(tmp), manifest=manifest)
            self.assertEqual(m["version"], {"short": "2.5.1", "build": "999"})

    def test_tag_falls_back_to_the_artifact_describe_on_a_dry_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            # No --tag: the effective tag is the app manifest's git describe.
            m, _ = run(Path(tmp), tag=None)
            self.assertEqual(m["tag"], "v1.2.0")

    def test_missing_app_manifest_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "app-packaging").mkdir()
            with self.assertRaises(rel.ReleaseError) as caught:
                rel.generate(
                    tmp,
                    package_dir=tmp / "app-packaging",
                    out_dir=tmp / "release",
                    tag="v1.2.0",
                    runner=FakeGit(),
                    echo=lambda _: None,
                    environ={},
                )
            self.assertIn("app manifest not found", str(caught.exception))


class CredentialScrubTest(unittest.TestCase):
    def test_clean_release_passes_the_scrub(self):
        with tempfile.TemporaryDirectory() as tmp:
            # A normal run reaches the end (scrub is the last step and did not raise).
            m, _ = run(Path(tmp))
            self.assertEqual(m["credential_scrub"], "passed")

    def test_a_pem_key_in_any_file_fails_the_scrub(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "d"
            out.mkdir()
            (out / "leak.txt").write_text(
                "-----BEGIN PRIVATE KEY-----\nMIIabc\n-----END PRIVATE KEY-----\n"
            )
            findings = rel.scrub_findings(out)
            self.assertTrue(findings)
            with self.assertRaises(rel.ReleaseError):
                rel.assert_credential_free(out)

    def test_a_leak_word_in_structured_json_fails_but_not_in_the_changelog(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "d"
            out.mkdir()
            # A commit subject legitimately mentioning "password" is fine.
            (out / "CHANGELOG.md").write_text("- fix password reset flow (Dev)\n")
            self.assertEqual(rel.scrub_findings(out), [])
            # The same word inside structured JSON we author is a leak signal.
            (out / "release-manifest.json").write_text('{"api_hash": "deadbeef"}')
            self.assertTrue(rel.scrub_findings(out))

    def test_an_embedded_p12_style_blob_is_caught(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "d"
            out.mkdir()
            (out / "blob.json").write_text(json.dumps({"cert": "A" * 300}))
            self.assertTrue(rel.scrub_findings(out))


if __name__ == "__main__":
    unittest.main()
