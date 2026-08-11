#!/usr/bin/env python3
"""Tests for the task-scoped TDLib stories contract fixtures."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / ".scripts" / "tdlib" / "story_contract.py"
CONTRACT_PATH = (
    REPO_ROOT
    / ".scripts"
    / "tdlib"
    / "fixtures"
    / "TASK-260721-2e6sbq_story-schema-contract.json"
)
WIRE_PATH = (
    REPO_ROOT
    / ".scripts"
    / "tdlib"
    / "fixtures"
    / "TASK-260721-2e6sbq_story-wire-fixtures.ndjson"
)
PINNED_SCHEMA_PATH = (
    REPO_ROOT / ".temp" / "tdlib" / "src" / "td" / "generate" / "scheme" / "td_api.tl"
)


def load_module():
    spec = importlib.util.spec_from_file_location("story_contract", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


story_contract = load_module()


class StoryContractTests(unittest.TestCase):
    def setUp(self):
        self.contract = story_contract.load_json(CONTRACT_PATH)
        self.fixtures = story_contract.load_ndjson(WIRE_PATH)

    def test_fixtures_are_self_consistent(self):
        story_contract.validate_request_policy(self.contract)
        story_contract.validate_wire_fixtures(self.fixtures, self.contract)

    def test_pinned_schema_matches_reviewed_declarations(self):
        if not PINNED_SCHEMA_PATH.is_file():
            self.skipTest("pinned TDLib checkout is not present")
        schema_text = PINNED_SCHEMA_PATH.read_text(encoding="utf-8")
        story_contract.validate_schema(schema_text, self.contract)

    def test_schema_drift_is_rejected(self):
        if not PINNED_SCHEMA_PATH.is_file():
            self.skipTest("pinned TDLib checkout is not present")
        schema_text = PINNED_SCHEMA_PATH.read_text(encoding="utf-8")
        mutated = schema_text.replace(
            "openStory story_poster_chat_id:int53 story_id:int32 = Ok;",
            "openStory story_poster_chat_id:int53 story_id:int53 = Ok;",
        )
        mutated_contract = json.loads(json.dumps(self.contract))
        mutated_contract["schema"]["sha256"] = story_contract.schema_sha256(mutated)
        with self.assertRaisesRegex(story_contract.ContractError, "openStory"):
            story_contract.validate_schema(mutated, mutated_contract)

    def test_protected_placeholder_rejects_locator_leak(self):
        mutated = json.loads(json.dumps(self.fixtures))
        protected = next(
            fixture
            for fixture in mutated
            if fixture["fixture"] == "protected-story-placeholder"
        )
        protected["expected"]["locator"] = {"file_id": 599}
        with self.assertRaisesRegex(story_contract.ContractError, "leaks"):
            story_contract.validate_wire_fixtures(mutated, self.contract)

    def test_protected_placeholder_rejects_file_type_leak(self):
        mutated = json.loads(json.dumps(self.fixtures))
        protected = next(
            fixture
            for fixture in mutated
            if fixture["fixture"] == "protected-story-placeholder"
        )
        protected["expected"]["file_type"] = "fileTypeVideoStory"
        with self.assertRaisesRegex(story_contract.ContractError, "leaks"):
            story_contract.validate_wire_fixtures(mutated, self.contract)

    def test_unknown_remote_file_type_must_be_explicit_null(self):
        mutated = json.loads(json.dumps(self.fixtures))
        unknown = next(
            fixture
            for fixture in mutated
            if fixture["fixture"] == "remote-file-unknown"
        )
        unknown["wire"]["file_type"] = {"@type": "fileTypeUnknown"}
        with self.assertRaisesRegex(story_contract.ContractError, "JSON null"):
            story_contract.validate_wire_fixtures(mutated, self.contract)

    def test_locator_policy_drift_is_rejected(self):
        mutated = json.loads(json.dumps(self.contract))
        mutated["locator_file_type_policy"]["video-thumbnail"] = "fileTypeVideoStory"
        with self.assertRaisesRegex(story_contract.ContractError, "locator FileType policy"):
            story_contract.validate_locator_file_type_policy(mutated)

    def test_background_dispatcher_rejects_live_story_viewing_and_stream_calls(self):
        for method in (
            "getGroupCall",
            "joinLiveStory",
            "leaveGroupCall",
            "getGroupCallStreams",
            "getGroupCallStreamSegment",
        ):
            with self.subTest(method=method):
                self.assertFalse(
                    story_contract.background_request_allowed(self.contract, method)
                )

    def test_live_stream_method_cannot_escape_background_forbidden_policy(self):
        mutated = json.loads(json.dumps(self.contract))
        mutated["request_policy"]["forbidden_in_background_discovery"].remove(
            "getGroupCallStreams"
        )
        with self.assertRaisesRegex(
            story_contract.ContractError,
            "missing from background-forbidden policy",
        ):
            story_contract.validate_request_policy(mutated)

    def test_live_story_placeholder_cannot_schedule_background_request(self):
        mutated = json.loads(json.dumps(self.fixtures))
        live_story = next(
            fixture
            for fixture in mutated
            if fixture["fixture"] == "live-story-metadata-placeholder"
        )
        live_story["expected"]["background_requests"] = ["getGroupCallStreams"]
        with self.assertRaisesRegex(
            story_contract.ContractError,
            "must not issue group-call requests",
        ):
            story_contract.validate_wire_fixtures(mutated, self.contract)

    def test_admin_without_edit_stories_cannot_be_elevated(self):
        mutated = json.loads(json.dumps(self.fixtures))
        admin = next(
            fixture
            for fixture in mutated
            if fixture["fixture"] == "archive-rights-admin-no-edit"
        )
        admin["expected"].update(
            capability="manageable",
            eligible=True,
            archive_request="getChatArchivedStories",
        )
        with self.assertRaisesRegex(story_contract.ContractError, "classification drift"):
            story_contract.validate_wire_fixtures(mutated, self.contract)

    def test_cli_validation_accepts_fixture_only_mode(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            contract_copy = temp_path / "contract.json"
            wire_copy = temp_path / "wire.ndjson"
            contract_copy.write_text(CONTRACT_PATH.read_text(encoding="utf-8"))
            wire_copy.write_text(WIRE_PATH.read_text(encoding="utf-8"))
            counts = story_contract.validate(contract_copy, wire_copy)
        self.assertEqual(counts, (70, 25))


if __name__ == "__main__":
    unittest.main()
