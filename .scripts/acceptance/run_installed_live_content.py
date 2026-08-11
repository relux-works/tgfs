#!/usr/bin/env python3
"""Privacy-safe acceptance for an installed, authorized live-content profile.

The ``before`` phase selects a genuinely dataless and uncached attachment,
enumerates only its date-first ancestors, proves enumeration left it dataless
and uncached, and opens it exactly once. It also opens one current Markdown,
NDJSON, and chat metadata document, compares each with its verified generated
cache bytes, and checks generated storage against the configured quota. The
``after`` phase is run after an agent relaunch and repeats those generated
checks while comparing the sampled identity, the full active item set, and
every previously persisted history cursor.

Raw item/chat identifiers and content digests are kept only in the private
state file. The evidence file contains fixed labels, counts, booleans, timings,
versions, and signing state supplied by the caller.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
from collections.abc import Callable, Sequence


DEFAULT_DATA_ROOT = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive"
)
DEFAULT_CLOUD_ROOT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"
DEFAULT_STATE = Path(".temp/installed-live-content-private-state.json")
DEFAULT_EVIDENCE = Path(".temp/installed-live-content-evidence.json")
DEFAULT_CACHE_QUOTA_BYTES = 10_000_000_000
MAX_CANDIDATES = 512
HYDRATION_WAIT_ATTEMPTS = 100
HYDRATION_WAIT_SECONDS = 0.1
QUIESCENCE_ATTEMPTS = 30
QUIESCENCE_STABLE_POLLS = 3
QUIESCENCE_WAIT_SECONDS = 1.0
PUBLIC_EVIDENCE_FIELDS = {
    "active_stories_present",
    "after_cursor_count",
    "after_item_count",
    "app_version",
    "archive_mode",
    "authorized_account_count",
    "before_cursor_count",
    "before_item_count",
    "canonical_package_exit_code",
    "chat_json_present",
    "deep_signature_valid",
    "direct_month_present",
    "elapsed_ms",
    "failure_category",
    "finder_content_state",
    "finder_first_page_item_count",
    "generated_dates_stable",
    "generated_current_materializations_preserved",
    "generated_current_reference_count",
    "generated_document_open_count",
    "generated_exact_bytes_verified",
    "generated_metadata_stable",
    "generated_metadata_truthful",
    "generated_orphan_file_count",
    "generated_relaunch_exact_bytes_verified",
    "generated_storage_bytes",
    "generated_storage_within_quota",
    "hydrated_bytes_verified",
    "hydrated_size_matches",
    "hydration_count",
    "hydration_duration_ms",
    "hidden_chat_metadata_complete",
    "initial_enumeration_materialized_selected_media",
    "legacy_chat_metadata_absent",
    "messages_markdown_nonempty",
    "messages_ndjson_nonempty",
    "mounted_domain_count",
    "nonempty_story_chat_count",
    "prior_bundle_recoverable",
    "privacy_safe",
    "public_release_unchanged",
    "qualifying_chat_count",
    "quiescence_stable_poll_count",
    "relaunch_cursor_count_delta",
    "relaunch_cursor_missing_count",
    "relaunch_cursor_progress_preserved",
    "relaunch_cursor_progressed_count",
    "relaunch_cursor_regressed_count",
    "relaunch_hydration_preserved",
    "relaunch_item_count_delta",
    "relaunch_item_count_stable",
    "relaunch_item_identity_stable",
    "relaunch_item_set_additive_only",
    "relaunch_item_set_stable",
    "relaunch_prior_item_identity_preserved",
    "relaunch_retention_preserved",
    "retention_mode",
    "sample_dataless_after_enumeration",
    "sample_dataless_before_enumeration",
    "sample_uncached_after_enumeration",
    "sample_uncached_before_enumeration",
    "signed_identifiers",
    "signing_team",
    "story_count_after",
    "story_count_before",
    "story_containers_truthful",
    "story_transition_observed",
    "tdjson_linked",
    "zero_story_chat_count",
    "zero_story_containers_omitted",
}
PUBLIC_STRING_FIELDS = {
    "app_version",
    "failure_category",
    "finder_content_state",
    "retention_mode",
    "signing_team",
}
PUBLIC_SIGNED_IDENTIFIERS = [
    "com.reluxworks.gramdrive",
    "com.reluxworks.gramdrive.agent",
    "com.reluxworks.gramdrive.fileprovider",
]


class AcceptanceFailure(RuntimeError):
    """A fixed-label live acceptance failure safe to report."""


@dataclass(frozen=True)
class Candidate:
    item_id: bytes
    expected_size: int
    month_id: bytes
    chat_id: bytes
    list_id: bytes
    markdown_id: bytes
    ndjson_id: bytes
    chat_json_id: bytes


@dataclass(frozen=True)
class CursorComparison:
    before_count: int
    after_count: int
    missing_count: int
    regressed_count: int
    progressed_count: int

    @property
    def preserved(self) -> bool:
        return self.missing_count == 0 and self.regressed_count == 0


@dataclass(frozen=True)
class ItemComparison:
    before_count: int
    after_count: int
    count_delta: int
    count_stable: bool
    set_stable: bool
    prior_items_preserved: bool
    additive_only: bool


@dataclass(frozen=True)
class GeneratedStorageFacts:
    current_reference_count: int
    physical_file_count: int
    physical_bytes: int
    orphan_file_count: int
    within_quota: bool
    current_materializations_preserved: bool


@dataclass(frozen=True)
class NamespaceFacts:
    visible_chat_count: int
    hidden_metadata_count: int
    legacy_metadata_count: int
    zero_story_chat_count: int
    zero_story_container_count: int
    nonempty_story_chat_count: int
    active_story_chat_count: int
    active_story_container_count: int
    empty_active_container_count: int

    @property
    def hidden_metadata_complete(self) -> bool:
        return (
            self.visible_chat_count > 0
            and self.hidden_metadata_count == self.visible_chat_count
        )

    @property
    def zero_story_containers_omitted(self) -> bool:
        return (
            self.zero_story_chat_count > 0
            and self.zero_story_container_count == 0
        )

    @property
    def story_containers_truthful(self) -> bool:
        return (
            self.nonempty_story_chat_count > 0
            and self.active_story_container_count == self.active_story_chat_count
            and self.empty_active_container_count == 0
        )


def connection(database: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{database}?mode=ro", uri=True)


def cache_verified(db: sqlite3.Connection, item_id: bytes) -> bool:
    return (
        db.execute(
            "SELECT count(*) FROM cache_entries "
            "WHERE item_id=? AND verification='verified'",
            (item_id,),
        ).fetchone()[0]
        == 1
    )


def verify_persisted_sample(
    db: sqlite3.Connection, previous: dict
) -> tuple[bytes, int, str]:
    """Verify the exact one-open sample retained in private state."""
    sample_item = bytes.fromhex(previous["sample_item"])
    expected_size = previous["expected_size"]
    hydrated_digest = previous["hydrated_digest"]
    cached = db.execute(
        "SELECT size, blob_hash FROM cache_entries "
        "WHERE item_id=? AND verification='verified'",
        (sample_item,),
    ).fetchone()
    if cached is None:
        raise AcceptanceFailure("hydrated-sample-cache-entry-missing")
    cached_size, cached_digest = cached
    if cached_size != expected_size:
        raise AcceptanceFailure("hydrated-sample-size-mismatch")
    if cached_digest.hex() != hydrated_digest:
        raise AcceptanceFailure("hydrated-sample-digest-mismatch")
    return sample_item, expected_size, hydrated_digest


def item_path(db: sqlite3.Connection, cloud_root: Path, item_id: bytes) -> Path:
    names: list[str] = []
    current: bytes | None = item_id
    while current is not None:
        row = db.execute(
            "SELECT parent_item_id, safe_name, kind FROM items WHERE item_id=?",
            (current,),
        ).fetchone()
        if row is None:
            raise AcceptanceFailure("projection-item-missing")
        parent, name, kind = row
        if kind != "account":
            names.append(name)
        current = parent
    return cloud_root.joinpath(*reversed(names))


def finder_dataless(path: Path) -> bool:
    try:
        result = subprocess.run(
            ("stat", "-f", "%Sf", str(path)),
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise AcceptanceFailure("finder-placeholder-stat-failed") from error
    return "dataless" in result.stdout.lower()


def read_placeholder_once(path: Path) -> tuple[str, int]:
    """Read one selected placeholder while keeping path/content out of evidence."""
    digest = hashlib.sha256()
    byte_count = 0
    try:
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
                byte_count += len(chunk)
    except OSError as error:
        raise AcceptanceFailure("placeholder-hydration-read-failed") from error
    return digest.hexdigest(), byte_count


def verify_generated_documents(
    db: sqlite3.Connection,
    cloud_root: Path,
    item_ids: Sequence[bytes],
) -> list[dict]:
    """Verify current cache bytes against Finder opens without public content evidence."""
    expected_mime = (
        "text/markdown",
        "application/x-ndjson",
        "application/json",
    )
    if len(item_ids) != len(expected_mime):
        raise AcceptanceFailure("generated-document-set-incomplete")
    records: list[dict] = []
    for item_id, mime in zip(item_ids, expected_mime, strict=True):
        row = db.execute(
            """
            SELECT i.mime_type, i.logical_size, i.content_version,
                   i.created_at_ms, i.modified_at_ms,
                   c.content_version, c.size, c.materialization_ref
            FROM items i
            JOIN cache_entries c ON c.item_id=i.item_id
            WHERE i.item_id=? AND i.deleted_at_ms IS NULL
              AND c.verification='verified'
            """,
            (item_id,),
        ).fetchone()
        if row is None:
            raise AcceptanceFailure("generated-current-materialization-missing")
        (
            item_mime,
            logical_size,
            item_version,
            created_at_ms,
            modified_at_ms,
            cache_version,
            cache_size,
            materialization_ref,
        ) = row
        if (
            item_mime != mime
            or logical_size is None
            or item_version is None
            or item_version != cache_version
            or logical_size != cache_size
            or modified_at_ms is None
            or not materialization_ref
        ):
            raise AcceptanceFailure("generated-metadata-contract-failed")
        expected_digest, expected_size = read_placeholder_once(
            Path(materialization_ref)
        )
        finder_digest, finder_size = read_placeholder_once(
            item_path(db, cloud_root, item_id)
        )
        if (
            expected_digest != finder_digest
            or expected_size != finder_size
            or expected_size != logical_size
        ):
            raise AcceptanceFailure("generated-exact-bytes-mismatch")
        records.append(
            {
                "item_id": item_id.hex(),
                "mime_type": item_mime,
                "logical_size": logical_size,
                "content_version": item_version,
                "created_at_ms": created_at_ms,
                "modified_at_ms": modified_at_ms,
                "digest": expected_digest,
            }
        )
    return records


def configured_cache_quota(data_root: Path) -> int:
    settings = data_root / "agent/settings.json"
    if not settings.exists():
        return DEFAULT_CACHE_QUOTA_BYTES
    try:
        value = json.loads(settings.read_text()).get(
            "cacheQuotaBytes", DEFAULT_CACHE_QUOTA_BYTES
        )
    except (OSError, json.JSONDecodeError) as error:
        raise AcceptanceFailure("generated-quota-settings-invalid") from error
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise AcceptanceFailure("generated-quota-settings-invalid")
    return value


def verify_generated_storage(
    db: sqlite3.Connection, data_root: Path
) -> GeneratedStorageFacts:
    """Account for managed generated generations without exposing their paths."""
    generated_root = data_root / "cache/generated"
    quota = configured_cache_quota(data_root)
    rows = db.execute(
        "SELECT materialization_ref, size FROM cache_entries "
        "WHERE kind='generated_doc' AND verification='verified'"
    ).fetchall()
    root = generated_root.resolve()
    referenced: set[Path] = set()
    preserved = True
    for materialization_ref, expected_size in rows:
        if not materialization_ref:
            preserved = False
            continue
        path = Path(materialization_ref)
        try:
            resolved = path.resolve(strict=True)
            stat = resolved.stat()
        except OSError:
            preserved = False
            continue
        if (
            not resolved.is_relative_to(root)
            or not stat.st_mode
            or not resolved.is_file()
            or stat.st_size != expected_size
        ):
            preserved = False
            continue
        referenced.add(resolved)

    physical: dict[Path, int] = {}
    if generated_root.exists():
        for path in generated_root.rglob("*"):
            if path.is_file() and path.name in {
                "Messages.md",
                "Messages.ndjson",
                "chat.json",
            }:
                resolved = path.resolve()
                physical[resolved] = resolved.stat().st_size
    orphan_count = len(set(physical) - referenced)
    physical_bytes = sum(physical.values())
    return GeneratedStorageFacts(
        current_reference_count=len(rows),
        physical_file_count=len(physical),
        physical_bytes=physical_bytes,
        orphan_file_count=orphan_count,
        within_quota=physical_bytes <= quota,
        current_materializations_preserved=preserved
        and referenced.issubset(physical)
        and len(referenced) <= len(rows),
    )


def candidate_rows(db: sqlite3.Connection) -> list[Candidate]:
    rows = db.execute(
        """
        SELECT att.item_id, att.logical_size, month.item_id, chat.item_id,
               list.item_id, md.item_id, nd.item_id, cj.item_id
        FROM items att
        JOIN items month
          ON month.item_id=att.parent_item_id AND month.kind='month_dir'
        JOIN items chat
          ON chat.item_id=month.parent_item_id AND chat.kind='chat'
        JOIN items list
          ON list.item_id=chat.parent_item_id AND list.kind='chat_list'
        JOIN items md
          ON md.parent_item_id=month.item_id AND md.safe_name='Messages.md'
        JOIN items nd
          ON nd.parent_item_id=month.item_id AND nd.safe_name='Messages.ndjson'
        JOIN items cj
          ON cj.parent_item_id=chat.item_id AND cj.safe_name='.chat.json'
        WHERE att.kind='attachment'
          AND att.availability='fetchable'
          AND att.logical_size > 0
          AND att.deleted_at_ms IS NULL
          AND md.logical_size > 0
          AND nd.logical_size > 0
          AND NOT EXISTS (
              SELECT 1 FROM cache_entries cached
              WHERE cached.item_id=att.item_id
                AND cached.verification='verified'
          )
        ORDER BY att.logical_size ASC, att.item_id
        LIMIT ?
        """,
        (MAX_CANDIDATES,),
    ).fetchall()
    return [Candidate(*row) for row in rows]


def namespace_facts(db: sqlite3.Connection) -> NamespaceFacts:
    """Aggregate provider-visible namespace truth without exposing identities."""
    row = db.execute(
        """
        WITH chat_facts AS (
            SELECT
                chat.item_id,
                (
                    SELECT count(*)
                    FROM items metadata
                    WHERE metadata.parent_item_id=chat.item_id
                      AND metadata.deleted_at_ms IS NULL
                      AND metadata.safe_name='.chat.json'
                ) AS hidden_metadata_count,
                (
                    SELECT count(*)
                    FROM items metadata
                    WHERE metadata.parent_item_id=chat.item_id
                      AND metadata.deleted_at_ms IS NULL
                      AND metadata.safe_name='chat.json'
                ) AS legacy_metadata_count,
                (
                    SELECT count(*)
                    FROM items active
                    WHERE active.parent_item_id=chat.item_id
                      AND active.deleted_at_ms IS NULL
                      AND active.kind='active_stories'
                ) AS active_container_count,
                (
                    SELECT count(*)
                    FROM items story
                    JOIN items active ON active.item_id=story.parent_item_id
                    WHERE active.parent_item_id=chat.item_id
                      AND active.deleted_at_ms IS NULL
                      AND active.kind='active_stories'
                      AND story.deleted_at_ms IS NULL
                      AND story.kind='story_appearance'
                ) AS active_story_count,
                (
                    SELECT count(*)
                    FROM items story
                    JOIN items month ON month.item_id=story.parent_item_id
                    WHERE month.parent_item_id=chat.item_id
                      AND month.deleted_at_ms IS NULL
                      AND month.kind='month_dir'
                      AND story.deleted_at_ms IS NULL
                      AND story.kind='story_appearance'
                ) AS persistent_story_count
            FROM items chat
            WHERE chat.kind='chat' AND chat.deleted_at_ms IS NULL
        )
        SELECT
            count(*),
            coalesce(sum(hidden_metadata_count), 0),
            coalesce(sum(legacy_metadata_count), 0),
            coalesce(sum(
                CASE WHEN active_story_count + persistent_story_count = 0
                     THEN 1 ELSE 0 END
            ), 0),
            coalesce(sum(
                CASE WHEN active_story_count + persistent_story_count = 0
                     THEN active_container_count ELSE 0 END
            ), 0),
            coalesce(sum(
                CASE WHEN active_story_count + persistent_story_count > 0
                     THEN 1 ELSE 0 END
            ), 0),
            coalesce(sum(CASE WHEN active_story_count > 0 THEN 1 ELSE 0 END), 0),
            coalesce(sum(active_container_count), 0),
            coalesce(sum(
                CASE WHEN active_container_count > 0 AND active_story_count = 0
                     THEN active_container_count ELSE 0 END
            ), 0)
        FROM chat_facts
        """
    ).fetchone()
    return NamespaceFacts(*row)


def select_uncached_dataless_candidate(
    db: sqlite3.Connection,
    cloud_root: Path,
    dataless_probe: Callable[[Path], bool] = finder_dataless,
) -> tuple[Candidate, Path]:
    for candidate in candidate_rows(db):
        path = item_path(db, cloud_root, candidate.item_id)
        if not cache_verified(db, candidate.item_id) and dataless_probe(path):
            return candidate, path
    raise AcceptanceFailure("no-fresh-uncached-dataless-placeholder")


def active_items(db: sqlite3.Connection) -> tuple[str, int, list[str]]:
    rows = db.execute(
        "SELECT hex(item_id) FROM items "
        "WHERE deleted_at_ms IS NULL ORDER BY item_id"
    ).fetchall()
    return (
        hashlib.sha256(repr(rows).encode()).hexdigest(),
        len(rows),
        [row[0] for row in rows],
    )


def cursor_rows(db: sqlite3.Connection) -> list[tuple[int, int, int, int | None, int | None, int]]:
    return db.execute(
        "SELECT account_id, namespace_version, chat_id, "
        "oldest_loaded_message_id, newest_loaded_message_id, history_complete "
        "FROM chat_sync_state ORDER BY account_id, namespace_version, chat_id"
    ).fetchall()


def aggregate(db: sqlite3.Connection) -> dict:
    item_digest, item_count, item_ids = active_items(db)
    cursors = cursor_rows(db)
    retention = list(
        db.execute(
            "SELECT retention_mode, archive_mode FROM accounts "
            "WHERE auth_state='authorized'"
        ).fetchone()
    )
    stories = list(
        db.execute(
            "SELECT count(*), "
            "sum(CASE WHEN location='active' THEN 1 ELSE 0 END), "
            "sum(CASE WHEN location='profile' THEN 1 ELSE 0 END) "
            "FROM story_appearances"
        ).fetchone()
    )
    return {
        "item_digest": item_digest,
        "item_count": item_count,
        "item_ids": item_ids,
        "cursors": cursors,
        "cursor_count": len(cursors),
        "retention": retention,
        "stories": stories,
    }


def compare_cursors(
    before: Sequence[Sequence[int | None]],
    after: Sequence[Sequence[int | None]],
) -> CursorComparison:
    """Compare contiguous history windows without exposing their identities."""
    before_by_key = {tuple(row[:3]): tuple(row[3:]) for row in before}
    after_by_key = {tuple(row[:3]): tuple(row[3:]) for row in after}
    missing = 0
    regressed = 0
    progressed = 0
    for key, prior in before_by_key.items():
        current = after_by_key.get(key)
        if current is None:
            missing += 1
            continue
        old_oldest, old_newest, old_complete = prior
        new_oldest, new_newest, new_complete = current
        bounds_preserved = (
            (old_oldest is None or (new_oldest is not None and new_oldest <= old_oldest))
            and (old_newest is None or (new_newest is not None and new_newest >= old_newest))
        )
        completeness_preserved = not bool(old_complete) or bool(new_complete)
        if not bounds_preserved or not completeness_preserved:
            regressed += 1
        elif current != prior:
            progressed += 1
    return CursorComparison(
        before_count=len(before_by_key),
        after_count=len(after_by_key),
        missing_count=missing,
        regressed_count=regressed,
        progressed_count=progressed,
    )


def compare_items(
    before_digest: str,
    before_count: int,
    before_ids: Sequence[str],
    after_digest: str,
    after_count: int,
    after_ids: Sequence[str],
) -> ItemComparison:
    delta = after_count - before_count
    prior_items_preserved = set(before_ids).issubset(after_ids)
    return ItemComparison(
        before_count=before_count,
        after_count=after_count,
        count_delta=delta,
        count_stable=delta == 0,
        set_stable=before_digest == after_digest,
        prior_items_preserved=prior_items_preserved,
        additive_only=prior_items_preserved and delta >= 0,
    )


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def validate_public_evidence(evidence: dict) -> None:
    unexpected = set(evidence) - PUBLIC_EVIDENCE_FIELDS
    if unexpected:
        raise AcceptanceFailure("public-evidence-field-not-allow-listed")
    if evidence.get("privacy_safe") is not True:
        raise AcceptanceFailure("public-evidence-not-marked-privacy-safe")
    for key, value in evidence.items():
        if isinstance(value, str) and key not in PUBLIC_STRING_FIELDS:
            raise AcceptanceFailure("public-evidence-string-not-allow-listed")
        if isinstance(value, list) and key != "signed_identifiers":
            raise AcceptanceFailure("public-evidence-list-not-allow-listed")
        if key == "signed_identifiers" and value != PUBLIC_SIGNED_IDENTIFIERS:
            raise AcceptanceFailure("public-evidence-identifiers-not-allow-listed")
        if isinstance(value, dict):
            raise AcceptanceFailure("public-evidence-object-not-allow-listed")


def run_before(
    database: Path,
    data_root: Path,
    cloud_root: Path,
    state_path: Path,
    evidence_path: Path,
    dataless_probe: Callable[[Path], bool] = finder_dataless,
) -> dict:
    started = time.monotonic()
    db = connection(database)
    candidate, attachment = select_uncached_dataless_candidate(
        db, cloud_root, dataless_probe
    )
    ancestor_ids = (candidate.list_id, candidate.chat_id, candidate.month_id)
    ancestor_paths = [cloud_root]
    ancestor_paths.extend(item_path(db, cloud_root, item_id) for item_id in ancestor_ids)
    markdown = item_path(db, cloud_root, candidate.markdown_id)
    ndjson = item_path(db, cloud_root, candidate.ndjson_id)
    pre_uncached = not cache_verified(db, candidate.item_id)
    pre_dataless = dataless_probe(attachment)
    db.close()
    if not pre_uncached or not pre_dataless:
        raise AcceptanceFailure("placeholder-precondition-changed")

    for path in ancestor_paths:
        tuple(os.scandir(path))

    after_enumeration = connection(database)
    post_uncached = not cache_verified(after_enumeration, candidate.item_id)
    post_dataless = dataless_probe(attachment)
    markdown_size = markdown.stat().st_size
    ndjson_size = ndjson.stat().st_size
    generated_records = verify_generated_documents(
        after_enumeration,
        cloud_root,
        (candidate.markdown_id, candidate.ndjson_id, candidate.chat_json_id),
    )
    generated_storage = verify_generated_storage(after_enumeration, data_root)
    after_enumeration.close()
    if not post_uncached or not post_dataless:
        raise AcceptanceFailure("enumeration-materialized-sampled-placeholder")

    hydration_started = time.monotonic()
    hydrated_digest, byte_count = read_placeholder_once(attachment)
    hydration_ms = round((time.monotonic() - hydration_started) * 1000)

    blob = None
    for _ in range(HYDRATION_WAIT_ATTEMPTS):
        check = connection(database)
        blob = check.execute(
            "SELECT blob_hash, size FROM cache_entries "
            "WHERE item_id=? AND verification='verified'",
            (candidate.item_id,),
        ).fetchone()
        check.close()
        if blob is not None:
            break
        time.sleep(HYDRATION_WAIT_SECONDS)
    if blob is None:
        raise AcceptanceFailure("hydration-cache-publication-timeout")

    current = connection(database)
    snapshot = aggregate(current)
    namespace = namespace_facts(current)
    current.close()
    private_state = {
        **snapshot,
        "sample_item": candidate.item_id.hex(),
        "expected_size": candidate.expected_size,
        "hydrated_digest": hydrated_digest,
        "generated_records": generated_records,
    }
    write_json(state_path, private_state)
    evidence = {
        "privacy_safe": True,
        "qualifying_chat_count": 1,
        "chat_json_present": namespace.hidden_metadata_complete,
        "active_stories_present": namespace.active_story_container_count > 0,
        "hidden_chat_metadata_complete": namespace.hidden_metadata_complete,
        "legacy_chat_metadata_absent": namespace.legacy_metadata_count == 0,
        "zero_story_chat_count": namespace.zero_story_chat_count,
        "zero_story_containers_omitted": namespace.zero_story_containers_omitted,
        "nonempty_story_chat_count": namespace.nonempty_story_chat_count,
        "story_containers_truthful": namespace.story_containers_truthful,
        "direct_month_present": True,
        "messages_markdown_nonempty": markdown_size > 0,
        "messages_ndjson_nonempty": ndjson_size > 0,
        "sample_uncached_before_enumeration": pre_uncached,
        "sample_dataless_before_enumeration": pre_dataless,
        "sample_uncached_after_enumeration": post_uncached,
        "sample_dataless_after_enumeration": post_dataless,
        "initial_enumeration_materialized_selected_media": not (
            post_uncached and post_dataless
        ),
        "hydration_count": 1,
        "generated_document_open_count": len(generated_records),
        "generated_exact_bytes_verified": True,
        "generated_metadata_truthful": True,
        "generated_storage_bytes": generated_storage.physical_bytes,
        "generated_storage_within_quota": generated_storage.within_quota,
        "generated_orphan_file_count": generated_storage.orphan_file_count,
        "generated_current_reference_count": (
            generated_storage.current_reference_count
        ),
        "generated_current_materializations_preserved": (
            generated_storage.current_materializations_preserved
        ),
        "hydrated_size_matches": byte_count == candidate.expected_size == blob[1],
        "hydrated_bytes_verified": blob[0].hex() == hydrated_digest,
        "hydration_duration_ms": hydration_ms,
        "before_item_count": snapshot["item_count"],
        "before_cursor_count": snapshot["cursor_count"],
        "retention_mode": snapshot["retention"][0],
        "archive_mode": bool(snapshot["retention"][1]),
        "story_count_before": snapshot["stories"][0],
        "elapsed_ms": round((time.monotonic() - started) * 1000),
    }
    write_json(evidence_path, evidence)
    return evidence


def run_after(
    database: Path,
    data_root: Path,
    cloud_root: Path,
    state_path: Path,
    evidence_path: Path,
) -> dict:
    previous = json.loads(state_path.read_text())
    db = connection(database)
    current = aggregate(db)
    namespace = namespace_facts(db)
    sample_item = bytes.fromhex(previous["sample_item"])
    sampled_identity_stable = (
        db.execute(
            "SELECT count(*) FROM items WHERE item_id=? AND deleted_at_ms IS NULL",
            (sample_item,),
        ).fetchone()[0]
        == 1
    )
    blob = db.execute(
        "SELECT blob_hash, size FROM cache_entries "
        "WHERE item_id=? AND verification='verified'",
        (sample_item,),
    ).fetchone()
    prior_generated = previous["generated_records"]
    current_generated = verify_generated_documents(
        db,
        cloud_root,
        tuple(bytes.fromhex(record["item_id"]) for record in prior_generated),
    )
    generated_storage = verify_generated_storage(db, data_root)
    db.close()

    cursors = compare_cursors(previous["cursors"], current["cursors"])
    items = compare_items(
        previous["item_digest"],
        previous["item_count"],
        previous["item_ids"],
        current["item_digest"],
        current["item_count"],
        current["item_ids"],
    )
    evidence = json.loads(evidence_path.read_text())
    evidence.update(
        {
            "after_item_count": items.after_count,
            "relaunch_item_count_delta": items.count_delta,
            "relaunch_item_count_stable": items.count_stable,
            "relaunch_item_set_stable": items.set_stable,
            "relaunch_prior_item_identity_preserved": items.prior_items_preserved,
            "relaunch_item_set_additive_only": items.additive_only,
            "relaunch_item_identity_stable": sampled_identity_stable,
            "after_cursor_count": cursors.after_count,
            "relaunch_cursor_count_delta": cursors.after_count
            - cursors.before_count,
            "relaunch_cursor_missing_count": cursors.missing_count,
            "relaunch_cursor_regressed_count": cursors.regressed_count,
            "relaunch_cursor_progressed_count": cursors.progressed_count,
            "relaunch_cursor_progress_preserved": cursors.preserved,
            "relaunch_retention_preserved": current["retention"]
            == previous["retention"],
            "relaunch_hydration_preserved": blob is not None
            and blob[0].hex() == previous["hydrated_digest"]
            and blob[1] == previous["expected_size"],
            "generated_relaunch_exact_bytes_verified": True,
            "generated_storage_bytes": generated_storage.physical_bytes,
            "generated_storage_within_quota": generated_storage.within_quota,
            "generated_orphan_file_count": generated_storage.orphan_file_count,
            "generated_current_reference_count": (
                generated_storage.current_reference_count
            ),
            "generated_current_materializations_preserved": (
                generated_storage.current_materializations_preserved
            ),
            "generated_metadata_stable": all(
                current["mime_type"] == prior["mime_type"]
                and current["logical_size"] == prior["logical_size"]
                and current["content_version"] == prior["content_version"]
                for prior, current in zip(
                    prior_generated, current_generated, strict=True
                )
            ),
            "generated_dates_stable": all(
                current["created_at_ms"] == prior["created_at_ms"]
                and current["modified_at_ms"] == prior["modified_at_ms"]
                for prior, current in zip(
                    prior_generated, current_generated, strict=True
                )
            ),
            "story_count_after": current["stories"][0],
            "story_transition_observed": current["stories"]
            != previous["stories"],
            "chat_json_present": namespace.hidden_metadata_complete,
            "active_stories_present": namespace.active_story_container_count > 0,
            "hidden_chat_metadata_complete": namespace.hidden_metadata_complete,
            "legacy_chat_metadata_absent": namespace.legacy_metadata_count == 0,
            "zero_story_chat_count": namespace.zero_story_chat_count,
            "zero_story_containers_omitted": namespace.zero_story_containers_omitted,
            "nonempty_story_chat_count": namespace.nonempty_story_chat_count,
            "story_containers_truthful": namespace.story_containers_truthful,
        }
    )
    evidence["before_item_count"] = items.before_count
    evidence["before_cursor_count"] = cursors.before_count
    evidence["story_count_before"] = previous["stories"][0]
    write_json(evidence_path, evidence)
    return evidence


def run_stability_snapshot(
    database: Path, state_path: Path, evidence_path: Path
) -> dict:
    """Snapshot a quiescent item set around the already allowed hydration."""
    previous = json.loads(state_path.read_text())
    stable_polls = 0
    previous_items: tuple[str, int] | None = None
    snapshot = None
    for _ in range(QUIESCENCE_ATTEMPTS):
        db = connection(database)
        try:
            verify_persisted_sample(db, previous)
            snapshot = aggregate(db)
        finally:
            db.close()
        current_items = (snapshot["item_digest"], snapshot["item_count"])
        stable_polls = stable_polls + 1 if current_items == previous_items else 1
        previous_items = current_items
        if stable_polls >= QUIESCENCE_STABLE_POLLS:
            break
        time.sleep(QUIESCENCE_WAIT_SECONDS)
    else:
        raise AcceptanceFailure("active-item-set-did-not-quiesce")
    if snapshot is None:
        raise AcceptanceFailure("quiescent-snapshot-not-found")
    write_json(
        state_path,
        {
            **snapshot,
            "sample_item": previous["sample_item"],
            "expected_size": previous["expected_size"],
            "hydrated_digest": previous["hydrated_digest"],
            "generated_records": previous["generated_records"],
        },
    )
    evidence = json.loads(evidence_path.read_text())
    evidence.update(
        {
            "before_item_count": snapshot["item_count"],
            "before_cursor_count": snapshot["cursor_count"],
            "retention_mode": snapshot["retention"][0],
            "archive_mode": bool(snapshot["retention"][1]),
            "story_count_before": snapshot["stories"][0],
        }
    )
    write_json(evidence_path, evidence)
    return {
        "privacy_safe": True,
        "before_item_count": snapshot["item_count"],
        "before_cursor_count": snapshot["cursor_count"],
        "quiescence_stable_poll_count": stable_polls,
    }


def evidence_passed(phase: str, evidence: dict) -> bool:
    before_fields = (
        "chat_json_present",
        "direct_month_present",
        "messages_markdown_nonempty",
        "messages_ndjson_nonempty",
        "sample_uncached_before_enumeration",
        "sample_dataless_before_enumeration",
        "sample_uncached_after_enumeration",
        "sample_dataless_after_enumeration",
        "hydrated_size_matches",
        "hydrated_bytes_verified",
        "generated_exact_bytes_verified",
        "generated_metadata_truthful",
        "generated_storage_within_quota",
        "generated_current_materializations_preserved",
        "hidden_chat_metadata_complete",
        "legacy_chat_metadata_absent",
        "zero_story_containers_omitted",
        "story_containers_truthful",
    )
    if phase == "stability-snapshot":
        return True
    if not all(evidence.get(field) is True for field in before_fields):
        return False
    if evidence.get("initial_enumeration_materialized_selected_media") is not False:
        return False
    if evidence.get("hydration_count") != 1:
        return False
    if evidence.get("generated_document_open_count") != 3:
        return False
    if evidence.get("generated_orphan_file_count") != 0:
        return False
    if evidence.get("zero_story_chat_count", 0) < 1:
        return False
    if evidence.get("nonempty_story_chat_count", 0) < 1:
        return False
    if phase == "before":
        return True
    after_fields = (
        "relaunch_prior_item_identity_preserved",
        "relaunch_item_set_additive_only",
        "relaunch_item_identity_stable",
        "relaunch_cursor_progress_preserved",
        "relaunch_retention_preserved",
        "relaunch_hydration_preserved",
        "generated_relaunch_exact_bytes_verified",
        "generated_metadata_stable",
        "generated_dates_stable",
    )
    return all(evidence.get(field) is True for field in after_fields)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=("before", "stability-snapshot", "after"))
    parser.add_argument("--data-root", type=Path, default=DEFAULT_DATA_ROOT)
    parser.add_argument("--cloud-root", type=Path, default=DEFAULT_CLOUD_ROOT)
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--evidence", type=Path, default=DEFAULT_EVIDENCE)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    database = args.data_root / "state/gramdrive.sqlite3"
    try:
        if args.phase == "before":
            evidence = run_before(
                database, args.data_root, args.cloud_root, args.state, args.evidence
            )
        elif args.phase == "stability-snapshot":
            evidence = run_stability_snapshot(database, args.state, args.evidence)
        else:
            evidence = run_after(
                database, args.data_root, args.cloud_root, args.state, args.evidence
            )
    except (AcceptanceFailure, OSError, sqlite3.Error, ValueError, KeyError) as error:
        label = str(error) if isinstance(error, AcceptanceFailure) else "acceptance-io-failed"
        print(f"installed live-content acceptance failed: {label}", file=sys.stderr)
        return 1
    try:
        validate_public_evidence(evidence)
    except AcceptanceFailure as error:
        print(f"installed live-content acceptance failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(evidence, sort_keys=True))
    return 0 if evidence_passed(args.phase, evidence) else 1


if __name__ == "__main__":
    raise SystemExit(main())
