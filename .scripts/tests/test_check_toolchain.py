#!/usr/bin/env python3
"""Tests for .scripts/check_toolchain.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests

The checker's whole job is to notice when the pin is *not* in effect, which is
a state that cannot be staged on a correctly configured machine. So the version
probes are faked and the pin file is written into a temp dir: these tests are
about the comparison logic, not about this laptop's rustup.
"""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CHECKER_PATH = REPO_ROOT / ".scripts" / "check_toolchain.py"


def load_checker_module():
    spec = importlib.util.spec_from_file_location("check_toolchain", CHECKER_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


check_toolchain = load_checker_module()

PINNED = "1.91.0"

TOOLCHAIN_TOML = f"""
[toolchain]
channel = "{PINNED}"
components = ["rustfmt", "clippy"]
profile = "minimal"
"""

CARGO_TOML = """
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
rust-version = "1.91"
"""


def healthy_versions() -> dict[str, tuple[int, str]]:
    """Probe replies from a machine where the pin is correctly in effect."""
    return {
        "rustc --version": (0, f"rustc {PINNED} (f8297e351 2025-10-28)"),
        "cargo --version": (0, f"cargo {PINNED} (ea2d97820 2025-10-10)"),
        "cargo fmt --version": (0, "rustfmt 1.8.0-stable (f8297e35 2025-10-28)"),
        "cargo clippy --version": (0, f"clippy 0.1.91 (f8297e351 2025-10-28)"),
        "cargo deny --version": (0, "cargo-deny 0.20.2"),
    }


class VersionParsingTests(unittest.TestCase):
    def test_parses_each_tool_version_banner(self):
        cases = {
            f"rustc {PINNED} (f8297e351 2025-10-28)": PINNED,
            f"cargo {PINNED} (ea2d97820 2025-10-10)": PINNED,
            "cargo-deny 0.20.2": "0.20.2",
            "rustfmt 1.8.0-stable (f8297e35 2025-10-28)": "1.8.0",
        }
        for banner, expected in cases.items():
            with self.subTest(banner=banner):
                self.assertEqual(check_toolchain.parse_version(banner), expected)

    def test_unparsable_banner_returns_none(self):
        self.assertIsNone(check_toolchain.parse_version("error: no such command"))

    def test_two_component_channel_matches_its_patch_release(self):
        # A channel may be written "1.91"; that pins the same release as
        # "1.91.0" and must not be reported as a mismatch.
        self.assertTrue(check_toolchain.channel_matches("1.91", "1.91.0"))
        self.assertTrue(check_toolchain.channel_matches("1.91.0", "1.91.0"))

    def test_channel_does_not_match_a_different_release(self):
        self.assertFalse(check_toolchain.channel_matches("1.91.0", "1.92.0"))
        self.assertFalse(check_toolchain.channel_matches("1.91.0", "1.91.1"))
        self.assertFalse(check_toolchain.channel_matches("1.91", "1.90.0"))


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo_root = Path(self.tmp.name)
        self.write_pin(TOOLCHAIN_TOML)
        (self.repo_root / "Cargo.toml").write_text(CARGO_TOML)
        self.addCleanup(self.tmp.cleanup)

    def write_pin(self, text: str):
        (self.repo_root / "rust-toolchain.toml").write_text(text)

    def check_with(self, versions: dict[str, tuple[int, str]]) -> list[str]:
        def fake_run(argv, cwd):
            return versions.get(" ".join(argv), (127, "not found"))

        original = check_toolchain.run
        check_toolchain.run = fake_run
        try:
            return check_toolchain.check(self.repo_root)
        finally:
            check_toolchain.run = original

    def test_healthy_toolchain_reports_no_errors(self):
        self.assertEqual(self.check_with(healthy_versions()), [])

    def test_missing_pin_file_is_an_error(self):
        (self.repo_root / "rust-toolchain.toml").unlink()
        errors = self.check_with(healthy_versions())
        self.assertEqual(len(errors), 1)
        self.assertIn("not pinned", errors[0])

    def test_floating_channel_is_rejected(self):
        # "stable" resolves to a different compiler over time, which is the
        # exact non-determinism the pin exists to prevent.
        for channel in ("stable", "beta", "nightly", "nightly-2025-10-28"):
            with self.subTest(channel=channel):
                self.write_pin(f'[toolchain]\nchannel = "{channel}"\n')
                errors = self.check_with(healthy_versions())
                self.assertTrue(any("floating channel" in e for e in errors))

    def test_rustc_not_matching_the_pin_is_an_error(self):
        versions = healthy_versions()
        versions["rustc --version"] = (0, "rustc 1.85.0 (abc123 2025-01-01)")
        errors = self.check_with(versions)
        self.assertTrue(any("rustc" in e and "1.85.0" in e for e in errors))

    def test_cargo_not_matching_the_pin_is_an_error(self):
        versions = healthy_versions()
        versions["cargo --version"] = (0, "cargo 1.85.0 (abc123 2025-01-01)")
        errors = self.check_with(versions)
        self.assertTrue(any("cargo:" in e and "1.85.0" in e for e in errors))

    def test_missing_component_is_an_error(self):
        versions = healthy_versions()
        del versions["cargo clippy --version"]
        errors = self.check_with(versions)
        self.assertTrue(any("clippy" in e and "rustup component add" in e for e in errors))

    def test_msrv_above_the_pin_is_an_error(self):
        # Declaring support for a compiler newer than the one pinned is a
        # promise the workspace cannot keep.
        (self.repo_root / "Cargo.toml").write_text(
            CARGO_TOML.replace('rust-version = "1.91"', 'rust-version = "1.95"')
        )
        errors = self.check_with(healthy_versions())
        self.assertTrue(any("rust-version" in e and "1.95" in e for e in errors))

    def test_msrv_below_the_pin_is_fine(self):
        # Building a 1.85-compatible workspace with 1.91 is normal.
        (self.repo_root / "Cargo.toml").write_text(
            CARGO_TOML.replace('rust-version = "1.91"', 'rust-version = "1.85"')
        )
        self.assertEqual(self.check_with(healthy_versions()), [])

    def test_missing_cargo_deny_is_an_error_naming_the_install(self):
        versions = healthy_versions()
        del versions["cargo deny --version"]
        errors = self.check_with(versions)
        self.assertTrue(any("cargo-deny" in e and "brew install" in e for e in errors))

    def test_cargo_deny_below_the_minimum_is_an_error(self):
        versions = healthy_versions()
        versions["cargo deny --version"] = (0, "cargo-deny 0.14.0")
        errors = self.check_with(versions)
        self.assertTrue(any("cargo-deny" in e and "0.14.0" in e for e in errors))

    def test_cargo_deny_at_the_minimum_is_accepted(self):
        minimum = ".".join(str(part) for part in check_toolchain.MIN_CARGO_DENY)
        versions = healthy_versions()
        versions["cargo deny --version"] = (0, f"cargo-deny {minimum}")
        self.assertEqual(self.check_with(versions), [])


class RepoPinTests(unittest.TestCase):
    """The real pin file, not a fixture."""

    def test_repo_pins_an_exact_channel(self):
        import tomllib

        pin = tomllib.loads((REPO_ROOT / "rust-toolchain.toml").read_text())
        channel = pin["toolchain"]["channel"]
        self.assertRegex(channel, check_toolchain.EXACT_CHANNEL_RE)

    def test_repo_pins_the_components_the_gate_runs(self):
        import tomllib

        pin = tomllib.loads((REPO_ROOT / "rust-toolchain.toml").read_text())
        # The core suite runs `cargo fmt` and `cargo clippy`; a pin that omits
        # them leaves the gate dependent on whatever the host happens to have.
        self.assertIn("rustfmt", pin["toolchain"]["components"])
        self.assertIn("clippy", pin["toolchain"]["components"])

    def test_repo_msrv_matches_the_pinned_channel(self):
        import tomllib

        pin = tomllib.loads((REPO_ROOT / "rust-toolchain.toml").read_text())
        manifest = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text())
        msrv = manifest["workspace"]["package"]["rust-version"]
        self.assertTrue(
            check_toolchain.channel_matches(msrv, pin["toolchain"]["channel"]),
            "Cargo.toml rust-version and rust-toolchain.toml channel must be "
            "bumped together",
        )


if __name__ == "__main__":
    unittest.main()
