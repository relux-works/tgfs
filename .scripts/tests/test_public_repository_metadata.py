#!/usr/bin/env python3
"""Policy checks for the files that make the repository safe to publish."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]


class PublicRepositoryMetadataTests(unittest.TestCase):
    def read(self, relative_path: str) -> str:
        return (REPO_ROOT / relative_path).read_text(encoding="utf-8")

    def test_apache_license_and_notice_are_present(self):
        license_text = self.read("LICENSE")
        notice = self.read("NOTICE")
        self.assertIn("Apache License", license_text)
        self.assertIn("Version 2.0, January 2004", license_text)
        self.assertIn("GramDrive", notice)
        self.assertIn("Copyright 2026 Relux Works", notice)

    def test_community_and_security_documents_link_to_private_reporting(self):
        security = self.read("SECURITY.md")
        contributing = self.read("CONTRIBUTING.md")
        readme = self.read("README.md")
        self.assertIn("security/advisories/new", security)
        self.assertIn("Apache License 2.0", contributing)
        self.assertIn("SECURITY.md", contributing)
        self.assertIn("Apache License 2.0", readme)
        self.assertTrue((REPO_ROOT / "CODE_OF_CONDUCT.md").is_file())

    def test_public_readiness_prevents_legacy_release_feed_publication(self):
        readiness = self.read("docs/PUBLIC_REPOSITORY_READINESS.md")
        self.assertIn("v0.1.0", readiness)
        self.assertIn("v0.1.1", readiness)
        self.assertIn("Sparkle", readiness)
        self.assertIn("convert both\npublished releases to GitHub drafts", readiness)
        self.assertIn("Preserve both releases, their tags, and\ntheir assets", readiness)
        self.assertIn("cannot be accessed by unauthenticated users", readiness)
        self.assertIn("do not delete, archive, or remove any of them", readiness)
        self.assertTrue((REPO_ROOT / ".github" / "CODEOWNERS").is_file())
        self.assertTrue(
            (REPO_ROOT / ".github" / "ISSUE_TEMPLATE" / "bug_report.yml").is_file()
        )


if __name__ == "__main__":
    unittest.main()
