#!/usr/bin/env python3
"""Tests for the value-free update-secret inventory preflight."""
from __future__ import annotations

import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[2] / ".scripts/release/check_update_secret_inventory.py"
spec = importlib.util.spec_from_file_location("update_inventory", SCRIPT)
inventory = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = inventory
spec.loader.exec_module(inventory)


class InventoryTests(unittest.TestCase):
    def test_inventory_contains_exactly_seven_initial_names(self):
        self.assertEqual(sum(len(names) for names in inventory.EXPECTED.values()), 7)
        self.assertEqual(set(inventory.EXPECTED), {"updates-test", "release"})

    def test_compare_reports_missing_and_unexpected_names(self):
        missing, unexpected = inventory.compare({"MACOS_CERT_P12", "EXTRA"}, inventory.EXPECTED["updates-test"])
        self.assertIn("MACOS_CERT_PASSWORD", missing)
        self.assertEqual(unexpected, ["EXTRA"])

    def test_listed_names_uses_name_only_github_query(self):
        calls = []
        def runner(argv):
            calls.append(tuple(argv))
            return 0, json.dumps([{"name": "MACOS_CERT_P12"}])
        self.assertEqual(inventory.listed_names("updates-test", runner), {"MACOS_CERT_P12"})
        self.assertEqual(calls, [("gh", "secret", "list", "--env", "updates-test", "--json", "name")])

    def test_listed_names_rejects_non_json_response(self):
        with self.assertRaisesRegex(RuntimeError, "invalid name-only"):
            inventory.listed_names("release", lambda _: (0, "not json"))

    def test_set_secret_uses_stdin_and_maps_initial_and_versioned_names(self):
        calls = []
        value = b"test-only-secret-bytes"

        def runner(argv, stdin):
            calls.append((tuple(argv), stdin))
            return 0

        self.assertEqual(inventory.set_secret("MACOS_CERT_P12", value, runner), "updates-test")
        self.assertEqual(inventory.set_secret("SPARKLE_TEST_V12_EDDSA_PRIVATE_KEY_B64", value, runner), "updates-test")
        self.assertEqual(inventory.set_secret("SPARKLE_STABLE_V12_EDDSA_PRIVATE_KEY_B64", value, runner), "release")
        self.assertEqual(
            [argv for argv, _ in calls],
            [
                ("gh", "secret", "set", "MACOS_CERT_P12", "--env", "updates-test"),
                ("gh", "secret", "set", "SPARKLE_TEST_V12_EDDSA_PRIVATE_KEY_B64", "--env", "updates-test"),
                ("gh", "secret", "set", "SPARKLE_STABLE_V12_EDDSA_PRIVATE_KEY_B64", "--env", "release"),
            ],
        )
        self.assertTrue(all(value not in " ".join(argv).encode() for argv, _ in calls))
        self.assertTrue(all(stdin == value for _, stdin in calls))

    def test_every_initial_name_maps_to_its_declared_environment(self):
        for environment, names in inventory.EXPECTED.items():
            for name in names:
                self.assertEqual(inventory.environment_for(name), environment)

    def test_set_command_never_echoes_stdin_value(self):
        value = b"test-only-secret-bytes"
        stdin = type("Stdin", (), {"buffer": io.BytesIO(value)})()
        output = io.StringIO()
        with patch.object(inventory.sys, "stdin", stdin), redirect_stdout(output):
            self.assertEqual(inventory.main(["--set", "MACOS_CERT_PASSWORD"], setter=lambda _, received: int(received != value)), 0)
        self.assertEqual(output.getvalue(), "stored MACOS_CERT_PASSWORD in updates-test\n")
        self.assertNotIn(value.decode(), output.getvalue())

    def test_group_reader_requires_owner_only_directory_and_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            os.chmod(directory, 0o700)
            for name in inventory.NOTARY_NAMES:
                path = directory / name
                path.write_bytes(b"test-only-secret-bytes")
                os.chmod(path, 0o600)
            calls = []
            stored = inventory.set_group(directory, inventory.NOTARY_NAMES, lambda argv, value: calls.append((tuple(argv), value)) or 0)
        self.assertEqual([name for name, _ in stored], list(inventory.NOTARY_NAMES))
        self.assertEqual([argv[3] for argv, _ in calls], list(inventory.NOTARY_NAMES))
        self.assertTrue(all(b"test-only-secret-bytes" not in " ".join(argv).encode() for argv, _ in calls))

    def test_rejects_invalid_or_empty_secret_values(self):
        with self.assertRaisesRegex(ValueError, "unsupported"):
            inventory.environment_for("NOT_A_RELEASE_SECRET")
        with self.assertRaisesRegex(ValueError, "empty"):
            inventory.set_secret("MACOS_CERT_P12", b"")


if __name__ == "__main__":
    unittest.main()
