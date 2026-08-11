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

    def test_runbook_requires_versioned_sparkle_creation_export_escrow_and_cleanup(self):
        runbook = (SCRIPT.parents[2] / "docs/UPDATE_OPERATIONS.md").read_text()
        expected_sequences = (
            ("Test-V1", "test-v1", "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64"),
            ("Stable-V1", "stable-v1", "SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64"),
            ("Test-V2", "test-v2", "SPARKLE_TEST_V2_EDDSA_PRIVATE_KEY_B64"),
            ("Stable-V2", "stable-v2", "SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64"),
        )
        for generation, file_stem, secret_name in expected_sequences:
            account = f"GramDrive-Sparkle-{generation}"
            public_key = f'generate_keys --account {account} -p > "$SPARKLE_STAGE_DIR/{file_stem}.public"'
            export = f'generate_keys --account {account} -x "$SPARKLE_STAGE_DIR/{file_stem}.private"'
            escrow = f'mv "$SPARKLE_STAGE_DIR/{file_stem}.private" "$SPARKLE_ESCROW_DIR/{file_stem}.private"'
            setter = (
                f'base64 < "$SPARKLE_ESCROW_DIR/{file_stem}.private" '
                f'| python3 .scripts/release/check_update_secret_inventory.py --set {secret_name}'
            )
            self.assertIn(f"generate_keys --account {account} >/dev/null", runbook)
            self.assertIn(public_key, runbook)
            self.assertIn(export, runbook)
            self.assertIn(escrow, runbook)
            self.assertIn(setter, runbook)
            self.assertLess(runbook.index(public_key), runbook.index(export))
            self.assertLess(runbook.index(export), runbook.index(escrow))
            self.assertLess(runbook.index(escrow), runbook.index(setter))
        self.assertIn('rmdir "$SPARKLE_STAGE_DIR"', runbook)
        self.assertNotRegex(runbook, r"generate_keys\s+-x(?:\s|$)")

    def test_runbook_requires_public_key_cleanup_and_safe_retirement_ordering(self):
        runbook = (SCRIPT.parents[2] / "docs/UPDATE_OPERATIONS.md").read_text()
        public_cleanup_commands = (
            'rm "$SPARKLE_STAGE_DIR/test-v1.public" "$SPARKLE_STAGE_DIR/stable-v1.public"',
            'rm "$SPARKLE_STAGE_DIR/stable-v2.public"',
            'rm "$SPARKLE_STAGE_DIR/test-v2.public"',
        )
        for cleanup in public_cleanup_commands:
            self.assertIn(cleanup, runbook)

        retirement_blocks = (
            (
                "release",
                "SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64",
                "SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64",
                "GramDrive-Sparkle-Stable-V1",
                "stable-v1",
            ),
            (
                "updates-test",
                "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64",
                "SPARKLE_TEST_V2_EDDSA_PRIVATE_KEY_B64",
                "GramDrive-Sparkle-Test-V1",
                "test-v1",
            ),
        )
        for environment, old_secret, new_secret, account, file_stem in retirement_blocks:
            start = runbook.index(f'export SPARKLE_RETIRE_ENV={environment}')
            end = runbook.index("```", start)
            block = runbook[start:end]
            before = f'"$SPARKLE_STAGE_DIR/{file_stem}-before-retirement.json"'
            after = f'"$SPARKLE_STAGE_DIR/{file_stem}-after-retirement.json"'
            self.assertIn(f"export SPARKLE_OLD_SECRET={old_secret}", block)
            self.assertIn(f"export SPARKLE_NEW_SECRET={new_secret}", block)
            self.assertIn(f"export SPARKLE_OLD_ACCOUNT={account}", block)
            self.assertIn(f'gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > {before}', block)
            self.assertIn(f'gh secret delete "$SPARKLE_OLD_SECRET" --env "$SPARKLE_RETIRE_ENV"', block)
            self.assertIn(f'gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > {after}', block)
            self.assertIn('! grep -F "\\\"$SPARKLE_OLD_SECRET\\\""', block)
            self.assertIn('grep -F "\\\"$SPARKLE_NEW_SECRET\\\""', block)
            self.assertIn('security delete-generic-password -s https://sparkle-project.org -a "$SPARKLE_OLD_ACCOUNT"', block)
            self.assertIn(f'rm {before} {after}', block)
            self.assertIn('rmdir "$SPARKLE_STAGE_DIR"', block)

        stable_retirement = runbook.index("export SPARKLE_RETIRE_ENV=release")
        test_retirement = runbook.index("export SPARKLE_RETIRE_ENV=updates-test")
        stable_prerequisites = (
            "V2 secret was stored and encrypted escrow was verified",
            "V2-only update has passed on the old client",
            "old-key bridge URL is frozen",
        )
        for prerequisite in stable_prerequisites:
            self.assertLess(runbook.index(prerequisite), stable_retirement)
        test_prerequisites = (
            "old-key bridge URL is frozen and verified",
            "V2 secret is stored and encrypted escrow is verified",
            "old test V1 client has installed the bridge and passed a later V2-only update",
        )
        for prerequisite in test_prerequisites:
            self.assertLess(runbook.index(prerequisite), test_retirement)


if __name__ == "__main__":
    unittest.main()
