#!/usr/bin/env python3
"""Tests for the value-free update-secret inventory preflight."""
from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
