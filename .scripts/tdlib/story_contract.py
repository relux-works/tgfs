#!/usr/bin/env python3
"""Validate the task-scoped TDLib stories schema and synthetic wire contract.

This tool never starts TDLib and never talks to Telegram. It compares an
already-present td_api.tl checkout with the reviewed declaration fixture and
checks privacy invariants in synthetic TDJSON examples.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
DEFAULT_CONTRACT = (
    SCRIPT_DIR / "fixtures" / "TASK-260721-2e6sbq_story-schema-contract.json"
)
DEFAULT_WIRE_FIXTURES = (
    SCRIPT_DIR / "fixtures" / "TASK-260721-2e6sbq_story-wire-fixtures.ndjson"
)
DECLARATION_RE = re.compile(r"^([A-Za-z][A-Za-z0-9]*)\b.* = [A-Za-z][A-Za-z0-9]*;$")
PROTECTED_FORBIDDEN_KEYS = {
    "caption",
    "content",
    "content_locator",
    "file_id",
    "file_type",
    "local_path",
    "locator",
    "remote_id",
    "remote_unique_id",
    "text",
}
EXPECTED_LOCATOR_FILE_TYPE_POLICY = {
    "photo-size": "fileTypePhotoStory",
    "video-primary": "fileTypeVideoStory",
    "video-alternative": "fileTypeVideoStory",
    "video-thumbnail": "fileTypeThumbnail",
    "minithumbnail": "no_file_locator",
    "unknown": None,
}
LOCATOR_FIXTURE_ROLES = {
    "remote-file-photo-story": ("photo-size",),
    "remote-file-video-story": ("video-primary", "video-alternative"),
    "remote-file-thumbnail": ("video-thumbnail",),
    "remote-file-unknown": ("unknown",),
}
ARCHIVE_FIXTURE_IDS = {
    "archive-rights-owner",
    "archive-rights-owner-bot",
    "archive-rights-creator",
    "archive-rights-manageable",
    "archive-rights-admin-no-edit",
    "archive-rights-ordinary",
    "archive-rights-unavailable",
}
LIVE_METADATA_METHODS = {"getGroupCall"}
LIVE_VIEWER_METHODS = {
    "joinLiveStory",
    "leaveGroupCall",
    "getGroupCallStreams",
    "getGroupCallStreamSegment",
}
LIVE_REQUEST_FIXTURES = {
    "live-story-group-call-metadata-request": (
        "getGroupCall",
        "explicit_live_metadata_only",
    ),
    "live-story-join-request": ("joinLiveStory", "explicit_live_viewer_only"),
    "live-story-leave-request": ("leaveGroupCall", "explicit_live_viewer_only"),
    "live-story-streams-request": (
        "getGroupCallStreams",
        "explicit_live_viewer_only",
    ),
    "live-story-stream-segment-request": (
        "getGroupCallStreamSegment",
        "explicit_live_viewer_only",
    ),
}


class ContractError(ValueError):
    """The checked schema or fixture violates the reviewed contract."""


def load_json(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as stream:
        value = json.load(stream)
    if not isinstance(value, dict):
        raise ContractError(f"{path}: expected a JSON object")
    return value


def load_ndjson(path: Path) -> list[dict[str, Any]]:
    values: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ContractError(f"{path}:{line_number}: expected a JSON object")
            values.append(value)
    return values


def parse_declarations(schema_text: str) -> dict[str, str]:
    declarations: dict[str, str] = {}
    for raw_line in schema_text.splitlines():
        line = raw_line.strip()
        match = DECLARATION_RE.fullmatch(line)
        if match:
            declarations[match.group(1)] = line
    return declarations


def schema_sha256(schema_text: str) -> str:
    return hashlib.sha256(schema_text.encode("utf-8")).hexdigest()


def validate_schema(schema_text: str, contract: dict[str, Any]) -> None:
    expected_hash = contract["schema"]["sha256"]
    actual_hash = schema_sha256(schema_text)
    if actual_hash != expected_hash:
        raise ContractError(
            f"td_api.tl hash mismatch: expected {expected_hash}, got {actual_hash}"
        )

    declarations = parse_declarations(schema_text)
    for name, expected in contract["schema_declarations"].items():
        actual = declarations.get(name)
        if actual != expected:
            raise ContractError(
                f"schema declaration drift for {name}: expected {expected!r}, got {actual!r}"
            )

    absences = contract["schema_absences"]
    for name in absences["constructors"]:
        if name in declarations:
            raise ContractError(f"constructor expected absent from pinned schema: {name}")
    for token in absences["tokens"]:
        if re.search(rf"\b{re.escape(token)}\b", schema_text):
            raise ContractError(f"token expected absent from pinned schema: {token}")


def walk_keys(value: Any) -> set[str]:
    if isinstance(value, dict):
        result = set(value)
        for child in value.values():
            result.update(walk_keys(child))
        return result
    if isinstance(value, list):
        result: set[str] = set()
        for child in value:
            result.update(walk_keys(child))
        return result
    return set()


def validate_request_policy(contract: dict[str, Any]) -> None:
    policy = contract["request_policy"]
    allowed = set(policy["background_metadata_allowed"])
    hydration = set(policy["explicit_hydration_only"])
    live_metadata = set(policy["explicit_live_metadata_only"])
    live_viewer = set(policy["explicit_live_viewer_only"])
    forbidden = set(policy["forbidden_in_background_discovery"])
    overlaps = (allowed | hydration) & forbidden
    if overlaps:
        raise ContractError(f"request policy overlap: {sorted(overlaps)}")
    exclusive_overlap = (allowed | hydration) & (live_metadata | live_viewer)
    if exclusive_overlap:
        raise ContractError(
            f"request policy class overlap: {sorted(exclusive_overlap)}"
        )
    if live_metadata != LIVE_METADATA_METHODS:
        raise ContractError(
            "explicit live metadata policy must contain only getGroupCall"
        )
    if live_viewer != LIVE_VIEWER_METHODS:
        raise ContractError(
            "explicit live viewer policy must lock join, leave, stream catalog, "
            "and stream segment methods"
        )
    missing_from_forbidden = (live_metadata | live_viewer) - forbidden
    if missing_from_forbidden:
        raise ContractError(
            "live-story methods missing from background-forbidden policy: "
            f"{sorted(missing_from_forbidden)}"
        )
    if "openStory" not in forbidden or "closeStory" not in forbidden:
        raise ContractError("openStory and closeStory must both be background-forbidden")
    if "downloadFile" not in hydration:
        raise ContractError("downloadFile must be explicit-hydration-only")
    locked = set(contract["schema_declarations"])
    unlocked = (allowed | hydration | forbidden) - locked
    if unlocked:
        raise ContractError(f"request policy has unlocked declarations: {sorted(unlocked)}")


def background_request_allowed(contract: dict[str, Any], method: str) -> bool:
    """Return whether the background metadata dispatcher may encode a method."""

    validate_request_policy(contract)
    return method in set(contract["request_policy"]["background_metadata_allowed"])


def validate_locator_file_type_policy(contract: dict[str, Any]) -> None:
    policy = contract.get("locator_file_type_policy")
    if policy != EXPECTED_LOCATOR_FILE_TYPE_POLICY:
        raise ContractError(
            "locator FileType policy must lock story photo/video/thumbnail roles, "
            "no minithumbnail locator, and null for unknown"
        )
    locked = set(contract["schema_declarations"])
    required = {
        "fileTypePhotoStory",
        "fileTypeThumbnail",
        "fileTypeVideoStory",
        "getRemoteFile",
        "thumbnail",
    }
    missing = required - locked
    if missing:
        raise ContractError(
            f"locator FileType policy has unlocked declarations: {sorted(missing)}"
        )


def validate_archive_capability_policy(contract: dict[str, Any]) -> None:
    policy = contract.get("archive_capability_policy")
    if not isinstance(policy, dict):
        raise ContractError("archive capability policy must be a JSON object")
    required_paths = {
        ("owner", "current_user_type", "userTypeRegular"),
        ("owner", "target_chat_id", "getMe.id"),
        ("bot_account", "current_user_type", "userTypeBot"),
        ("manageable_creator", "member_id_type", "messageSenderUser"),
        ("manageable_creator", "status_type", "chatMemberStatusCreator"),
        (
            "manageable_administrator",
            "status_type",
            "chatMemberStatusAdministrator",
        ),
        (
            "manageable_administrator",
            "required_right",
            "rights.can_edit_stories",
        ),
    }
    for branch, field, expected in required_paths:
        if policy.get(branch, {}).get(field) != expected:
            raise ContractError(
                f"archive capability policy must lock {branch}.{field}={expected}"
            )
    locked = set(contract["schema_declarations"])
    required_declarations = {
        "chatMember",
        "chatMemberStatusAdministrator",
        "chatMemberStatusCreator",
        "messageSenderUser",
        "user",
        "userTypeBot",
        "userTypeRegular",
    }
    missing = required_declarations - locked
    if missing:
        raise ContractError(
            f"archive capability policy has unlocked declarations: {sorted(missing)}"
        )


def classify_archive_fixture(wire: dict[str, Any]) -> tuple[str, bool, str | None]:
    if wire.get("@type") == "error":
        return "unavailable", False, "archive_eligibility_unknown"

    current_user = wire.get("current_user")
    if isinstance(current_user, dict):
        user_type = current_user.get("type")
        user_type_name = user_type.get("@type") if isinstance(user_type, dict) else None
        if user_type_name == "userTypeBot":
            return "unavailable", False, "archive_unavailable_account_type"
        if (
            user_type_name == "userTypeRegular"
            and wire.get("chat_id") == current_user.get("id")
        ):
            return "owner", True, None
        return "unavailable", False, "archive_eligibility_unknown"

    if wire.get("@type") != "chatMember":
        return "unavailable", False, "archive_eligibility_unknown"
    member_id = wire.get("member_id")
    if not isinstance(member_id, dict) or member_id.get("@type") != "messageSenderUser":
        return "unavailable", False, "archive_eligibility_unknown"
    if not isinstance(member_id.get("user_id"), int):
        return "unavailable", False, "archive_eligibility_unknown"
    status = wire.get("status")
    if not isinstance(status, dict):
        return "unavailable", False, "archive_eligibility_unknown"
    status_type = status.get("@type")
    if status_type == "chatMemberStatusCreator":
        return "manageable", True, None
    if status_type == "chatMemberStatusAdministrator":
        rights = status.get("rights")
        if isinstance(rights, dict) and rights.get("can_edit_stories") is True:
            return "manageable", True, None
    return "ordinary", False, "archive_unavailable_rights"


def validate_wire_fixtures(
    fixtures: list[dict[str, Any]], contract: dict[str, Any]
) -> None:
    fixture_ids = [fixture.get("fixture") for fixture in fixtures]
    if len(fixture_ids) != len(set(fixture_ids)):
        raise ContractError("wire fixture identifiers must be unique")
    missing = set(contract["required_wire_fixtures"]) - set(fixture_ids)
    if missing:
        raise ContractError(f"missing required wire fixtures: {sorted(missing)}")

    by_id = {fixture["fixture"]: fixture for fixture in fixtures}
    protected = by_id["protected-story-placeholder"]
    if protected["wire"].get("can_be_forwarded") is not False:
        raise ContractError("protected fixture must have can_be_forwarded=false")
    leaked = walk_keys(protected["expected"]) & PROTECTED_FORBIDDEN_KEYS
    if leaked:
        raise ContractError(
            f"protected normalized placeholder leaks content fields: {sorted(leaked)}"
        )

    active_key = by_id["active-photo-full-story"]["expected"]["canonical_key"]
    profile_key = by_id["profile-transition-update"]["expected"]["canonical_key"]
    if active_key != profile_key:
        raise ContractError("active-to-profile transition changed the canonical story key")

    live_metadata = by_id["live-story-metadata-placeholder"]
    live_content = live_metadata["wire"].get("content")
    if not isinstance(live_content, dict) or live_content.get("@type") != "storyContentLive":
        raise ContractError("live story fixture must contain storyContentLive")
    live_expected = live_metadata["expected"]
    if live_expected.get("placeholder") != "live_story_viewer_unavailable":
        raise ContractError("live story must fail closed to a viewer-unavailable placeholder")
    if live_expected.get("persist_bytes") is not False:
        raise ContractError("live story metadata placeholder must not persist stream bytes")
    if live_expected.get("background_requests") != []:
        raise ContractError("live story metadata discovery must not issue group-call requests")

    request_policy = contract["request_policy"]
    for fixture_id, (method, expected_class) in LIVE_REQUEST_FIXTURES.items():
        fixture = by_id[fixture_id]
        wire = fixture["wire"]
        expected = fixture["expected"]
        if wire.get("@type") != method:
            raise ContractError(f"{fixture_id}: expected an exact {method} request")
        if method not in request_policy[expected_class]:
            raise ContractError(f"{fixture_id}: request policy classification drift")
        if expected.get("request_class") != expected_class:
            raise ContractError(f"{fixture_id}: expected request class drift")
        if expected.get("background_allowed") is not False:
            raise ContractError(f"{fixture_id}: live request must reject background use")
        if background_request_allowed(contract, method):
            raise ContractError(f"{fixture_id}: background dispatcher accepted {method}")

    locator_policy = contract["locator_file_type_policy"]
    for fixture_id, roles in LOCATOR_FIXTURE_ROLES.items():
        fixture = by_id[fixture_id]
        wire = fixture["wire"]
        if wire.get("@type") != "getRemoteFile":
            raise ContractError(f"{fixture_id}: expected a getRemoteFile request")
        if "file_type" not in wire:
            raise ContractError(f"{fixture_id}: getRemoteFile file_type must be explicit")
        file_type = wire.get("file_type")
        actual_file_type = (
            file_type.get("@type") if isinstance(file_type, dict) else None
        )
        expected_types = {locator_policy[role] for role in roles}
        if expected_types == {None} and file_type is not None:
            raise ContractError(f"{fixture_id}: unknown file_type must be JSON null")
        if expected_types != {None} and not isinstance(file_type, dict):
            raise ContractError(f"{fixture_id}: known file_type must be a TDJSON object")
        if len(expected_types) != 1 or actual_file_type not in expected_types:
            raise ContractError(
                f"{fixture_id}: locator role and getRemoteFile file_type disagree"
            )
        if fixture["expected"].get("file_type") != actual_file_type:
            raise ContractError(f"{fixture_id}: expected file_type is not wire-exact")
        if fixture["expected"].get("explicit_hydration_only") is not True:
            raise ContractError(f"{fixture_id}: getRemoteFile must be hydration-only")

    for fixture_id in ARCHIVE_FIXTURE_IDS:
        fixture = by_id[fixture_id]
        capability, eligible, placeholder = classify_archive_fixture(fixture["wire"])
        expected = fixture["expected"]
        if expected.get("capability") != capability or expected.get("eligible") != eligible:
            raise ContractError(f"{fixture_id}: archive capability classification drift")
        expected_request = "getChatArchivedStories" if eligible else None
        if expected.get("archive_request") != expected_request:
            raise ContractError(f"{fixture_id}: archive request eligibility drift")
        if not eligible and expected.get("placeholder") != placeholder:
            raise ContractError(f"{fixture_id}: archive placeholder classification drift")

    for fixture in fixtures:
        expected = fixture.get("expected")
        wire = fixture.get("wire")
        if not isinstance(expected, dict) or not isinstance(wire, dict):
            raise ContractError(
                f"{fixture.get('fixture')}: wire and expected must be JSON objects"
            )


def validate(
    contract_path: Path,
    wire_path: Path,
    schema_path: Path | None = None,
) -> tuple[int, int]:
    contract = load_json(contract_path)
    fixtures = load_ndjson(wire_path)
    validate_request_policy(contract)
    validate_locator_file_type_policy(contract)
    validate_archive_capability_policy(contract)
    validate_wire_fixtures(fixtures, contract)
    if schema_path is not None:
        schema_text = schema_path.read_text(encoding="utf-8")
        validate_schema(schema_text, contract)
    return len(contract["schema_declarations"]), len(fixtures)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--schema", type=Path, help="path to the pinned td_api.tl")
    parser.add_argument("--contract", type=Path, default=DEFAULT_CONTRACT)
    parser.add_argument("--wire-fixtures", type=Path, default=DEFAULT_WIRE_FIXTURES)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    declaration_count, fixture_count = validate(
        args.contract, args.wire_fixtures, args.schema
    )
    schema_status = " and pinned schema" if args.schema else ""
    print(
        f"validated {declaration_count} declarations and {fixture_count} "
        f"synthetic wire fixtures{schema_status}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
