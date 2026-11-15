#!/usr/bin/env python3
"""Hosted core CI contract and gitleaks installer integrity regressions."""

from __future__ import annotations

import hashlib
import io
import os
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/ci.yml"


def installer_script() -> str:
    block = WORKFLOW.read_text().split("      - name: Install gitleaks", 1)[1]
    return textwrap.dedent(block.split("        run: |\n", 1)[1].split("\n      - name:", 1)[0])


class HostedCIWorkflowTests(unittest.TestCase):
    # These assertions protect the required check names, hosted runner choice,
    # pinned native asset, and existing acceptance/provenance guarantees.
    def test_hosted_jobs_keep_pinned_tools_and_acceptance_gates(self):
        workflow = WORKFLOW.read_text()
        self.assertEqual(workflow.count("runs-on: macos-latest"), 2)
        self.assertNotIn("runs-on: [self-hosted", workflow)
        self.assertIn("    name: rust-core\n", workflow)
        self.assertIn("    name: secret-scan\n", workflow)
        self.assertIn("run: .github/scripts/bootstrap-rust-toolchain.sh", workflow)
        self.assertIn("tool: cargo-deny@0.20.2", workflow)
        self.assertIn("GITLEAKS_VERSION: 8.30.1", workflow)
        self.assertIn("GITLEAKS_SHA256: b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5", workflow)
        self.assertIn("darwin_arm64.tar.gz", installer_script())
        self.assertIn("fetch-depth: 0", workflow)
        for suite, run_id in (("all", "ci-all"), ("security", "ci-security")):
            self.assertIn(f"--suite {suite} --require-clean --run-id {run_id}", workflow)
            self.assertIn(f"path: .temp/acceptance/{run_id}", workflow)
        self.assertEqual(workflow.count("if: always()"), 2)
        self.assertEqual(workflow.count("if-no-files-found: error"), 2)
        self.assertEqual(workflow.count("retention-days: 14"), 2)

    # Execute the actual workflow shell with a local fixture archive, real
    # SHA-256 verification and tar extraction. This proves installer ordering
    # and PATH publication, not the upstream binary's macOS compatibility.
    def test_verified_archive_installs_and_publishes_path(self):
        self._run_installer(tampered=False)

    # A mismatched digest must stop before extraction, executable invocation,
    # or PATH publication; the valid archive above prevents reject-all success.
    def test_checksum_mismatch_never_installs_or_publishes_path(self):
        self._run_installer(tampered=True)

    def _run_installer(self, *, tampered: bool) -> None:
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            payload = b"#!/bin/sh\nprintf '8.30.1\\n'\n"
            archive = temp / "fixture.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                entry = tarfile.TarInfo("gitleaks")
                entry.size = len(payload)
                entry.mode = 0o755
                bundle.addfile(entry, io.BytesIO(payload))
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            if tampered:
                # Keep a valid tarball: without the checksum gate this must
                # extract and execute, rather than failing at decompression.
                with tarfile.open(archive, "w:gz") as bundle:
                    bundle.addfile(entry, io.BytesIO(payload.replace(b"8.30.1", b"8.30.2")))
            fake_bin = temp / "bin"
            fake_bin.mkdir()
            curl = fake_bin / "curl"
            curl.write_text('#!/bin/sh\nset -eu\ntest "$1" = -sSfL\n'
                            'test "$2" = "https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_darwin_arm64.tar.gz"\n'
                            'test "$3" = -o\ncp "$FIXTURE_ARCHIVE" "$4"\n')
            curl.chmod(0o755)
            github_path = temp / "github-path"
            github_path.touch()
            environment = {
                **os.environ,
                "HOME": str(temp / "home"),
                "PATH": f"{fake_bin}:{os.environ['PATH']}",
                "GITHUB_PATH": str(github_path),
                "GITLEAKS_VERSION": "8.30.1",
                "GITLEAKS_SHA256": digest,
                "FIXTURE_ARCHIVE": str(archive),
            }
            result = subprocess.run(
                ["bash", "-c", installer_script()], cwd=temp,
                env=environment, text=True, capture_output=True,
            )
            installed = temp / "home/.local/bin/gitleaks"
            if tampered:
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("checksum did NOT match", result.stderr)
                self.assertFalse(installed.exists())
                self.assertEqual(github_path.read_text(), "")
            else:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(installed.is_file())
                self.assertIn("8.30.1", result.stdout)
                self.assertEqual(github_path.read_text().splitlines(), [str(installed.parent)])
                self.assertFalse((temp / "gitleaks.tar.gz").exists())


if __name__ == "__main__":
    unittest.main()
