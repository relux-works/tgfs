#!/usr/bin/env python3
"""Regression tests for the non-interactive self-hosted Rust bootstrap."""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
BOOTSTRAP = REPO_ROOT / ".github" / "scripts" / "bootstrap-rust-toolchain.sh"


class BootstrapRustToolchainTests(unittest.TestCase):
    def test_all_rust_workflow_jobs_use_shared_bootstrap(self):
        expected_invocations = {
            ".github/workflows/ci.yml": 1,
            ".github/workflows/native-ci.yml": 3,
        }

        for relative_path, expected_count in expected_invocations.items():
            with self.subTest(workflow=relative_path):
                workflow = (REPO_ROOT / relative_path).read_text()
                self.assertEqual(
                    workflow.count("run: .github/scripts/bootstrap-rust-toolchain.sh"),
                    expected_count,
                )
                self.assertNotIn("run: rustup show", workflow)

    def test_clean_runner_bootstraps_pin_and_second_run_is_idempotent(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            workspace = self._workspace(temp)
            fake_bin = temp / "bin"
            fake_bin.mkdir()
            log = temp / "commands.log"
            github_path = temp / "github-path"
            github_path.touch()
            self._write_fake_curl(fake_bin / "curl")
            self._write_fake_shasum(fake_bin / "shasum")

            environment = self._environment(temp, fake_bin, github_path, log)
            first = subprocess.run(
                [str(BOOTSTRAP)], cwd=workspace, env=environment, text=True, capture_output=True
            )
            second = subprocess.run(
                [str(BOOTSTRAP)], cwd=workspace, env=environment, text=True, capture_output=True
            )

            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            commands = log.read_text().splitlines()
            self.assertEqual(commands.count("curl"), 1)
            self.assertEqual(commands.count("checksum verify"), 1)
            self.assertEqual(commands.count("toolchain install 1.91.0 --profile minimal"), 2)
            self.assertEqual(commands.count("component add --toolchain 1.91.0 rustfmt"), 2)
            self.assertEqual(commands.count("component add --toolchain 1.91.0 clippy"), 2)
            self.assertEqual(github_path.read_text().splitlines(), [str(temp / "home" / ".cargo" / "bin")])
            bootstrap = BOOTSTRAP.read_text()
            self.assertNotRegex(bootstrap, r"(?m)^[^#\n]*\bsource[ \t]")
            self.assertIn("--no-modify-path", bootstrap)
            self.assertIn("rustup_version=\"1.29.0\"", bootstrap)
            self.assertIn("rustup/archive/${rustup_version}", bootstrap)
            self.assertIn("33cf85df9142bc6d29cbc62fa5ca1d4c29622cddb55213a4c1a43c457fb9b2d7", bootstrap)
            self.assertIn("aeb4105778ca1bd3c6b0e75768f581c656633cd51368fa61289b6a71696ac7e1", bootstrap)
            self.assertIn("shasum -a 256 -c -", bootstrap)

    def test_bad_rustup_checksum_never_executes_the_download(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            workspace = self._workspace(temp)
            fake_bin = temp / "bin"
            fake_bin.mkdir()
            log = temp / "commands.log"
            github_path = temp / "github-path"
            github_path.touch()
            self._write_fake_curl(fake_bin / "curl")
            self._write_fake_shasum(fake_bin / "shasum")

            environment = self._environment(temp, fake_bin, github_path, log)
            environment["FAIL_INTEGRITY"] = "1"
            result = subprocess.run(
                [str(BOOTSTRAP)], cwd=workspace, env=environment, text=True, capture_output=True
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum verification failed", result.stderr)
            self.assertEqual(log.read_text().splitlines(), ["curl", "checksum verify"])

    def _workspace(self, temp: Path) -> Path:
        workspace = temp / "workspace"
        workspace.mkdir()
        (workspace / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.91.0"\ncomponents = ["rustfmt", "clippy"]\nprofile = "minimal"\n'
        )
        return workspace

    def _environment(self, temp: Path, fake_bin: Path, github_path: Path, log: Path) -> dict[str, str]:
        return {
            "HOME": str(temp / "home"),
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "GITHUB_PATH": str(github_path),
            "TEST_LOG": str(log),
        }

    def _write_fake_curl(self, path: Path) -> None:
        path.write_text(
            """#!/bin/sh
set -eu
printf '%s\\n' curl >> "$TEST_LOG"
output=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then output="$2"; shift 2; continue; fi
  shift
done
cat > "$output" <<'INSTALLER'
#!/bin/sh
set -eu
printf '%s\\n' installer >> "$TEST_LOG"
mkdir -p "$CARGO_HOME/bin"
cat > "$CARGO_HOME/bin/rustup" <<'RUSTUP'
#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$TEST_LOG"
case "$1" in
  toolchain|component|default|show) exit 0 ;;
esac
RUSTUP
cat > "$CARGO_HOME/bin/rustc" <<'RUSTC'
#!/bin/sh
echo 'rustc 1.91.0 (test)'
RUSTC
cat > "$CARGO_HOME/bin/cargo" <<'CARGO'
#!/bin/sh
echo 'cargo 1.91.0 (test)'
CARGO
chmod 0755 "$CARGO_HOME/bin/rustup" "$CARGO_HOME/bin/rustc" "$CARGO_HOME/bin/cargo"
INSTALLER
"""
        )
        path.chmod(0o755)

    def _write_fake_shasum(self, path: Path) -> None:
        path.write_text(
            """#!/bin/sh
set -eu
test "$1" = "-a"
test "$2" = "256"
test "$3" = "-c"
test "$4" = "-"
checksum_line="$(cat)"
test -n "$checksum_line"
test ! -x "${checksum_line#*  }"
printf '%s\\n' 'checksum verify' >> "$TEST_LOG"
test "${FAIL_INTEGRITY:-}" != "1"
"""
        )
        path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
