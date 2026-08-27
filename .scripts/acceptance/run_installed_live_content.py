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
from contextlib import contextmanager
from dataclasses import dataclass
from enum import Enum
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import time
from collections.abc import Callable, Iterator, Sequence


DEFAULT_DATA_ROOT = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive"
)
DEFAULT_CLOUD_ROOT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"
DEFAULT_STATE = Path(".temp/installed-live-content-private-state.json")
DEFAULT_EVIDENCE = Path(".temp/installed-live-content-evidence.json")
DEFAULT_CACHE_QUOTA_BYTES = 10_000_000_000
MAX_CANDIDATES = 20
PLACEHOLDER_PROBE_TIMEOUT_SECONDS = 0.5
SF_DATALESS = 0x40000000
HYDRATION_WAIT_ATTEMPTS = 100
HYDRATION_WAIT_SECONDS = 0.1
QUIESCENCE_ATTEMPTS = 30
QUIESCENCE_STABLE_POLLS = 3
QUIESCENCE_WAIT_SECONDS = 1.0
DEFAULT_OVERALL_DEADLINE_SECONDS = 120.0
WORKER_CLEANUP_RESERVE_SECONDS = 2.0
WORKER_TERM_GRACE_SECONDS = 0.5
MAX_GENERATED_SCAN_ENTRIES = 1_000_000
SNAPSHOT_SCHEMA_VERSION = 1
PHASE_STAGES = {
    "before": (
        "select_candidate",
        "enumerate_ancestors",
        "verify_generated_documents",
        "verify_generated_storage",
        "hydrate_attachment",
        "publish_hydration",
        "snapshot_identity_and_cursors",
        "verify_namespace",
        "write_evidence",
    ),
    "stability-snapshot": (
        "load_private_state",
        "verify_persisted_sample",
        "snapshot_identity_and_cursors",
        "write_evidence",
    ),
    "after": (
        "load_private_state",
        "snapshot_current_state",
        "verify_hydration",
        "verify_generated_documents",
        "verify_generated_storage",
        "compare_identity",
        "compare_cursors",
        "verify_namespace",
        "write_evidence",
    ),
}
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
    "deadline_ms",
    "deadline_remaining_ms",
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
    "generated_scan_entry_count",
    "generated_scan_entry_limit",
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
    "stage_timings_ms",
    "tdjson_linked",
    "zero_story_chat_count",
    "zero_story_containers_omitted",
    "timeout_stage",
    "worker_exit_code",
    "child_cleanup_complete",
    "phase",
    "placeholder_candidates_considered",
    "placeholder_dataless_count",
    "placeholder_materialized_count",
    "placeholder_missing_count",
    "placeholder_path_mismatch_count",
    "placeholder_probe_elapsed_ms",
    "placeholder_stat_error_count",
    "placeholder_stat_timeout_count",
}
PUBLIC_STRING_FIELDS = {
    "app_version",
    "failure_category",
    "finder_content_state",
    "retention_mode",
    "signing_team",
    "phase",
    "timeout_stage",
}
PUBLIC_SIGNED_IDENTIFIERS = [
    "com.reluxworks.gramdrive",
    "com.reluxworks.gramdrive.agent",
    "com.reluxworks.gramdrive.fileprovider",
]


class AcceptanceFailure(RuntimeError):
    """A fixed-label live acceptance failure safe to report."""

    def __init__(self, category: str, public_evidence: dict[str, int] | None = None):
        super().__init__(category)
        self.public_evidence = public_evidence or {}


class DeadlineExceeded(AcceptanceFailure):
    """The fixed overall acceptance budget was exhausted."""


class Deadline:
    def __init__(self, seconds: float | None) -> None:
        self.started = time.monotonic()
        self.end = None if seconds is None else self.started + seconds

    def check(self) -> None:
        if self.end is not None and time.monotonic() >= self.end:
            raise DeadlineExceeded("overall-deadline-exceeded")

    def remaining_ms(self) -> int | None:
        if self.end is None:
            return None
        return max(0, round((self.end - time.monotonic()) * 1000))

    def sqlite_progress(self) -> int:
        return int(self.end is not None and time.monotonic() >= self.end)


class StageRecorder:
    """Persist only fixed stage labels and aggregate timings while work runs."""

    def __init__(
        self,
        phase: str,
        deadline: Deadline,
        progress_path: Path | None = None,
    ) -> None:
        self.phase = phase
        self.deadline = deadline
        self.progress_path = progress_path
        self.timings: dict[str, int | None] = {
            stage: None for stage in PHASE_STAGES[phase]
        }
        self.current_stage: str | None = None
        self.current_started: float | None = None
        self.failed_stage: str | None = None
        self._persist()

    def _persist(self) -> None:
        if self.progress_path is None:
            return
        current_elapsed = None
        if self.current_started is not None:
            current_elapsed = round((time.monotonic() - self.current_started) * 1000)
        write_json(
            self.progress_path,
            {
                "phase": self.phase,
                "current_stage": self.current_stage,
                "current_stage_elapsed_ms": current_elapsed,
                # This private progress file is removed by the parent. The
                # monotonic value lets the parent derive a truthful duration
                # when the worker is killed while blocked inside a stage.
                "current_stage_started_monotonic_ns": (
                    None
                    if self.current_started is None
                    else round(self.current_started * 1_000_000_000)
                ),
                "stage_timings_ms": self.timings,
            },
        )

    @contextmanager
    def stage(self, name: str) -> Iterator[None]:
        if name not in self.timings:
            raise ValueError("unknown fixed acceptance stage")
        self.deadline.check()
        self.current_stage = name
        self.current_started = time.monotonic()
        self._persist()
        try:
            yield
            self.deadline.check()
        except BaseException:
            self.failed_stage = name
            raise
        finally:
            if self.current_started is not None:
                self.timings[name] = round(
                    (time.monotonic() - self.current_started) * 1000
                )
            self._persist()
            self.current_stage = None
            self.current_started = None

    def decorate(self, evidence: dict, deadline_ms: int) -> dict:
        evidence.update(
            {
                "phase": self.phase,
                "deadline_ms": deadline_ms,
                "deadline_remaining_ms": self.deadline.remaining_ms(),
                "stage_timings_ms": dict(self.timings),
                "timeout_stage": "none",
                "failure_category": "none",
            }
        )
        return evidence


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


class PlaceholderState(Enum):
    DATALESS = "dataless"
    MATERIALIZED = "materialized"
    MISSING = "missing"
    PLATFORM_ERROR = "platform-error"
    TIMEOUT = "timeout"


@dataclass(frozen=True)
class PlaceholderProbeResult:
    state: PlaceholderState
    elapsed_ms: int


@dataclass
class PlaceholderSelectionFacts:
    candidates_considered: int = 0
    dataless_count: int = 0
    materialized_count: int = 0
    missing_count: int = 0
    path_mismatch_count: int = 0
    stat_error_count: int = 0
    stat_timeout_count: int = 0
    probe_elapsed_ms: int = 0

    def record(self, result: PlaceholderProbeResult) -> None:
        self.candidates_considered += 1
        self.probe_elapsed_ms += result.elapsed_ms
        if result.state is PlaceholderState.DATALESS:
            self.dataless_count += 1
        elif result.state is PlaceholderState.MATERIALIZED:
            self.materialized_count += 1
        elif result.state is PlaceholderState.MISSING:
            self.missing_count += 1
        elif result.state is PlaceholderState.PLATFORM_ERROR:
            self.stat_error_count += 1
        elif result.state is PlaceholderState.TIMEOUT:
            self.stat_timeout_count += 1

    def public_evidence(self) -> dict[str, int]:
        return {
            "placeholder_candidates_considered": self.candidates_considered,
            "placeholder_dataless_count": self.dataless_count,
            "placeholder_materialized_count": self.materialized_count,
            "placeholder_missing_count": self.missing_count,
            "placeholder_path_mismatch_count": self.path_mismatch_count,
            "placeholder_probe_elapsed_ms": self.probe_elapsed_ms,
            "placeholder_stat_error_count": self.stat_error_count,
            "placeholder_stat_timeout_count": self.stat_timeout_count,
        }


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
    scan_entry_count: int
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
        return self.zero_story_chat_count > 0 and self.zero_story_container_count == 0

    @property
    def story_containers_truthful(self) -> bool:
        return (
            self.nonempty_story_chat_count > 0
            and self.active_story_container_count == self.active_story_chat_count
            and self.empty_active_container_count == 0
        )


def connection(database: Path, deadline: Deadline | None = None) -> sqlite3.Connection:
    db = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=2)
    if deadline is not None:
        db.set_progress_handler(deadline.sqlite_progress, 10_000)
    return db


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


def finder_placeholder_probe(path: Path) -> PlaceholderProbeResult:
    """Read only ``st_flags`` in a bounded child and return a fixed state."""
    started = time.monotonic()
    probe = (
        "import os,sys; "
        "\ntry: print(os.lstat(sys.argv[1]).st_flags)"
        "\nexcept FileNotFoundError: raise SystemExit(3)"
        "\nexcept OSError: raise SystemExit(4)"
    )
    try:
        result = subprocess.run(
            (sys.executable, "-c", probe, str(path)),
            check=False,
            capture_output=True,
            text=True,
            timeout=PLACEHOLDER_PROBE_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return PlaceholderProbeResult(
            PlaceholderState.TIMEOUT,
            round((time.monotonic() - started) * 1000),
        )
    except OSError:
        return PlaceholderProbeResult(
            PlaceholderState.PLATFORM_ERROR,
            round((time.monotonic() - started) * 1000),
        )
    elapsed_ms = round((time.monotonic() - started) * 1000)
    if result.returncode == 3:
        return PlaceholderProbeResult(PlaceholderState.MISSING, elapsed_ms)
    if result.returncode != 0:
        return PlaceholderProbeResult(PlaceholderState.PLATFORM_ERROR, elapsed_ms)
    try:
        flags = int(result.stdout.strip())
    except ValueError:
        return PlaceholderProbeResult(PlaceholderState.PLATFORM_ERROR, elapsed_ms)
    state = (
        PlaceholderState.DATALESS
        if flags & SF_DATALESS
        else PlaceholderState.MATERIALIZED
    )
    return PlaceholderProbeResult(state, elapsed_ms)


def finder_dataless(path: Path) -> bool:
    """Compatibility boolean for callers that need one strict probe."""
    result = finder_placeholder_probe(path)
    if result.state is PlaceholderState.DATALESS:
        return True
    if result.state is PlaceholderState.MATERIALIZED:
        return False
    raise AcceptanceFailure(f"finder-placeholder-stat-{result.state.value}")


def require_dataless_probe(
    path: Path,
    probe: Callable[[Path], bool | PlaceholderProbeResult],
) -> bool:
    result = probe(path)
    if isinstance(result, bool):
        return result
    if result.state is PlaceholderState.DATALESS:
        return True
    if result.state is PlaceholderState.MATERIALIZED:
        return False
    raise AcceptanceFailure(f"finder-placeholder-stat-{result.state.value}")


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
    db: sqlite3.Connection,
    data_root: Path,
    deadline: Deadline | None = None,
    scan_entry_limit: int = MAX_GENERATED_SCAN_ENTRIES,
) -> GeneratedStorageFacts:
    """Incrementally prove generated storage truth with bounded memory and work.

    Current references stream through a temporary indexed table instead of a
    Python set. Physical generations are enumerated with an explicit entry cap
    and deadline instead of an unbounded recursive ``Path.rglob``. Every
    physical generated file is matched by an indexed point lookup.
    """
    deadline = deadline or Deadline(None)
    generated_root = data_root / "cache/generated"
    quota = configured_cache_quota(data_root)
    root = generated_root.resolve()
    db.execute("DROP TABLE IF EXISTS temp.acceptance_generated_refs")
    db.execute(
        """
        CREATE TEMP TABLE acceptance_generated_refs (
            path TEXT PRIMARY KEY,
            expected_size INTEGER NOT NULL,
            size_consistent INTEGER NOT NULL DEFAULT 1,
            found INTEGER NOT NULL DEFAULT 0
        ) WITHOUT ROWID
        """
    )
    current_reference_count, missing_materialization_count = db.execute(
        """
        SELECT count(*), coalesce(sum(materialization_ref IS NULL), 0)
        FROM cache_entries
        WHERE kind='generated_doc' AND verification='verified'
        """
    ).fetchone()
    rows = db.execute(
        """
        SELECT materialization_ref, size
        FROM cache_entries INDEXED BY cache_entries_by_materialization_ref
        WHERE materialization_ref IS NOT NULL
          AND kind='generated_doc' AND verification='verified'
        ORDER BY materialization_ref
        """
    )
    preserved = missing_materialization_count == 0
    for materialization_ref, expected_size in rows:
        deadline.check()
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
        db.execute(
            """
            INSERT INTO acceptance_generated_refs(path, expected_size)
            VALUES (?, ?)
            ON CONFLICT(path) DO UPDATE SET
                size_consistent = size_consistent
                    AND expected_size = excluded.expected_size
            """,
            (str(resolved), expected_size),
        )

    physical_file_count = 0
    physical_bytes = 0
    orphan_count = 0
    scan_entry_count = 0
    if generated_root.exists():
        pending = [generated_root]
        while pending:
            deadline.check()
            directory = pending.pop()
            try:
                entries = os.scandir(directory)
            except OSError as error:
                raise AcceptanceFailure("generated-cache-inventory-failed") from error
            with entries:
                for entry in entries:
                    deadline.check()
                    scan_entry_count += 1
                    if scan_entry_count > scan_entry_limit:
                        raise AcceptanceFailure("generated-cache-entry-limit-exceeded")
                    if entry.is_dir(follow_symlinks=False):
                        pending.append(Path(entry.path))
                        continue
                    if not entry.is_file(follow_symlinks=False) or entry.name not in {
                        "Messages.md",
                        "Messages.ndjson",
                        "chat.json",
                    }:
                        continue
                    path = Path(entry.path)
                    try:
                        resolved = path.resolve(strict=True)
                        size = entry.stat(follow_symlinks=False).st_size
                    except OSError as error:
                        raise AcceptanceFailure(
                            "generated-cache-inventory-failed"
                        ) from error
                    physical_file_count += 1
                    physical_bytes += size
                    matched = db.execute(
                        """
                        SELECT expected_size, size_consistent
                        FROM acceptance_generated_refs WHERE path=?
                        """,
                        (str(resolved),),
                    ).fetchone()
                    if matched is None:
                        orphan_count += 1
                    else:
                        if matched[0] != size or not matched[1]:
                            preserved = False
                        db.execute(
                            "UPDATE acceptance_generated_refs SET found=1 WHERE path=?",
                            (str(resolved),),
                        )
    missing_reference_count, inconsistent_reference_count = db.execute(
        """
        SELECT
            coalesce(sum(CASE WHEN found=0 THEN 1 ELSE 0 END), 0),
            coalesce(sum(CASE WHEN size_consistent=0 THEN 1 ELSE 0 END), 0)
        FROM acceptance_generated_refs
        """
    ).fetchone()
    preserved = (
        preserved and missing_reference_count == 0 and inconsistent_reference_count == 0
    )
    return GeneratedStorageFacts(
        current_reference_count=current_reference_count,
        physical_file_count=physical_file_count,
        physical_bytes=physical_bytes,
        orphan_file_count=orphan_count,
        scan_entry_count=scan_entry_count,
        within_quota=physical_bytes <= quota,
        current_materializations_preserved=preserved,
    )


CANDIDATE_QUERY = """
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
        """


def candidate_rows(db: sqlite3.Connection) -> list[Candidate]:
    rows = db.execute(CANDIDATE_QUERY, (MAX_CANDIDATES,)).fetchall()
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
    dataless_probe: Callable[
        [Path], bool | PlaceholderProbeResult
    ] = finder_placeholder_probe,
) -> tuple[Candidate, Path, PlaceholderSelectionFacts]:
    facts = PlaceholderSelectionFacts()
    for candidate in candidate_rows(db):
        try:
            path = item_path(db, cloud_root, candidate.item_id)
        except AcceptanceFailure:
            facts.candidates_considered += 1
            facts.path_mismatch_count += 1
            continue
        if cache_verified(db, candidate.item_id):
            continue
        raw_result = dataless_probe(path)
        result = (
            raw_result
            if isinstance(raw_result, PlaceholderProbeResult)
            else PlaceholderProbeResult(
                (
                    PlaceholderState.DATALESS
                    if raw_result
                    else PlaceholderState.MATERIALIZED
                ),
                0,
            )
        )
        facts.record(result)
        try:
            refreshed_path = item_path(db, cloud_root, candidate.item_id)
        except AcceptanceFailure:
            facts.path_mismatch_count += 1
            continue
        if refreshed_path != path:
            facts.path_mismatch_count += 1
            continue
        if result.state is PlaceholderState.DATALESS:
            return candidate, path, facts
    category = (
        "no-fresh-uncached-dataless-placeholder"
        if facts.candidates_considered == 0
        else "bounded-placeholder-selection-exhausted"
    )
    raise AcceptanceFailure(category, facts.public_evidence())


def scalar_aggregate(db: sqlite3.Connection) -> dict:
    """Read aggregate facts only; bulk identities stay inside SQLite."""
    item_count = db.execute(
        "SELECT count(*) FROM items WHERE deleted_at_ms IS NULL"
    ).fetchone()[0]
    cursor_count = db.execute("SELECT count(*) FROM chat_sync_state").fetchone()[0]
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
        "item_count": item_count,
        "cursor_count": cursor_count,
        "retention": retention,
        "stories": stories,
    }


def snapshot_database_path(state_path: Path) -> Path:
    return state_path.with_name(f"{state_path.name}.snapshot.sqlite3")


def _snapshot_build_path(snapshot_path: Path) -> Path:
    return snapshot_path.with_name(f"{snapshot_path.name}.building")


def cleanup_incomplete_private_artifacts(state_path: Path) -> None:
    """Remove only fixed-name incomplete files that a killed worker can leave."""
    state_path.with_name(f"{state_path.name}.writing").unlink(missing_ok=True)
    snapshot = snapshot_database_path(state_path)
    candidates = [
        _snapshot_build_path(snapshot),
        snapshot.with_name(f"{snapshot.name}.poll-a"),
        snapshot.with_name(f"{snapshot.name}.poll-b"),
    ]
    candidates.extend(_snapshot_build_path(path) for path in candidates[1:])
    for candidate in candidates:
        for suffix in ("", "-shm", "-wal", "-journal"):
            Path(f"{candidate}{suffix}").unlink(missing_ok=True)


def create_indexed_snapshot(
    database: Path,
    snapshot_path: Path,
    deadline: Deadline | None = None,
) -> dict:
    """Copy bulk proof keys directly SQLite-to-SQLite with bounded memory."""
    deadline = deadline or Deadline(None)
    snapshot_path.parent.mkdir(parents=True, exist_ok=True)
    building = _snapshot_build_path(snapshot_path)
    for suffix in ("", "-shm", "-wal", "-journal"):
        Path(f"{building}{suffix}").unlink(missing_ok=True)
    snapshot = sqlite3.connect(building, timeout=2, uri=True)
    os.chmod(building, 0o600)
    snapshot.set_progress_handler(deadline.sqlite_progress, 10_000)
    try:
        deadline.check()
        snapshot.execute("PRAGMA journal_mode=DELETE")
        snapshot.execute("PRAGMA synchronous=FULL")
        snapshot.execute(
            "ATTACH DATABASE ? AS live",
            (f"file:{database}?mode=ro",),
        )
        deadline.check()
        snapshot.executescript(
            """
            CREATE TABLE metadata (
                schema_version INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE active_items (
                item_id BLOB NOT NULL PRIMARY KEY
            ) STRICT, WITHOUT ROWID;
            CREATE TABLE cursors (
                account_id INTEGER NOT NULL,
                namespace_version INTEGER NOT NULL,
                chat_id INTEGER NOT NULL,
                oldest_loaded_message_id INTEGER,
                newest_loaded_message_id INTEGER,
                history_complete INTEGER NOT NULL,
                PRIMARY KEY (account_id, namespace_version, chat_id)
            ) STRICT, WITHOUT ROWID;
            """
        )
        snapshot.execute("BEGIN IMMEDIATE")
        snapshot.execute(
            "INSERT INTO metadata VALUES (?, ?)",
            (SNAPSHOT_SCHEMA_VERSION, round(time.time() * 1000)),
        )
        deadline.check()
        snapshot.execute(
            """
            INSERT INTO active_items(item_id)
            SELECT item_id FROM live.items WHERE deleted_at_ms IS NULL
            """
        )
        deadline.check()
        snapshot.execute(
            """
            INSERT INTO cursors(
                account_id, namespace_version, chat_id,
                oldest_loaded_message_id, newest_loaded_message_id,
                history_complete
            )
            SELECT account_id, namespace_version, chat_id,
                   oldest_loaded_message_id, newest_loaded_message_id,
                   history_complete
            FROM live.chat_sync_state
            """
        )
        deadline.check()
        counts = {
            "item_count": snapshot.execute(
                "SELECT count(*) FROM active_items"
            ).fetchone()[0],
            "cursor_count": snapshot.execute("SELECT count(*) FROM cursors").fetchone()[
                0
            ],
        }
        snapshot.commit()
        snapshot.execute("DETACH DATABASE live")
        snapshot.close()
        os.chmod(building, 0o600)
        os.replace(building, snapshot_path)
        return counts
    except BaseException as error:
        snapshot.close()
        for suffix in ("", "-shm", "-wal", "-journal"):
            Path(f"{building}{suffix}").unlink(missing_ok=True)
        if isinstance(error, sqlite3.Error) and deadline.remaining_ms() == 0:
            raise DeadlineExceeded("overall-deadline-exceeded") from error
        raise


def attach_snapshot(db: sqlite3.Connection, snapshot_path: Path) -> None:
    if not snapshot_path.is_file():
        raise AcceptanceFailure("private-snapshot-missing")
    db.execute(
        "ATTACH DATABASE ? AS acceptance_snapshot",
        (f"file:{snapshot_path}?mode=ro",),
    )
    metadata = db.execute(
        "SELECT schema_version FROM acceptance_snapshot.metadata"
    ).fetchone()
    if metadata != (SNAPSHOT_SCHEMA_VERSION,):
        raise AcceptanceFailure("private-snapshot-schema-invalid")


def compare_items_indexed(db: sqlite3.Connection) -> ItemComparison:
    before_count = db.execute(
        "SELECT count(*) FROM acceptance_snapshot.active_items"
    ).fetchone()[0]
    after_count = db.execute(
        "SELECT count(*) FROM items WHERE deleted_at_ms IS NULL"
    ).fetchone()[0]
    missing = db.execute(
        """
        SELECT count(*)
        FROM acceptance_snapshot.active_items prior
        WHERE NOT EXISTS (
            SELECT 1 FROM items current
            WHERE current.item_id=prior.item_id
              AND current.deleted_at_ms IS NULL
        )
        """
    ).fetchone()[0]
    delta = after_count - before_count
    prior_items_preserved = missing == 0
    return ItemComparison(
        before_count=before_count,
        after_count=after_count,
        count_delta=delta,
        count_stable=delta == 0,
        set_stable=delta == 0 and prior_items_preserved,
        prior_items_preserved=prior_items_preserved,
        additive_only=prior_items_preserved and delta >= 0,
    )


def compare_cursors_indexed(db: sqlite3.Connection) -> CursorComparison:
    before_count = db.execute(
        "SELECT count(*) FROM acceptance_snapshot.cursors"
    ).fetchone()[0]
    after_count = db.execute("SELECT count(*) FROM chat_sync_state").fetchone()[0]
    missing_count = db.execute(
        """
        SELECT count(*) FROM acceptance_snapshot.cursors prior
        WHERE NOT EXISTS (
            SELECT 1 FROM chat_sync_state current
            WHERE current.account_id=prior.account_id
              AND current.namespace_version=prior.namespace_version
              AND current.chat_id=prior.chat_id
        )
        """
    ).fetchone()[0]
    regressed_count, progressed_count = db.execute(
        """
        SELECT
            coalesce(sum(CASE WHEN
                (prior.oldest_loaded_message_id IS NOT NULL AND
                    (current.oldest_loaded_message_id IS NULL OR
                     current.oldest_loaded_message_id > prior.oldest_loaded_message_id))
                OR
                (prior.newest_loaded_message_id IS NOT NULL AND
                    (current.newest_loaded_message_id IS NULL OR
                     current.newest_loaded_message_id < prior.newest_loaded_message_id))
                OR
                (prior.history_complete <> 0 AND current.history_complete = 0)
                THEN 1 ELSE 0 END), 0),
            coalesce(sum(CASE WHEN
                NOT (
                    (prior.oldest_loaded_message_id IS NOT NULL AND
                        (current.oldest_loaded_message_id IS NULL OR
                         current.oldest_loaded_message_id > prior.oldest_loaded_message_id))
                    OR
                    (prior.newest_loaded_message_id IS NOT NULL AND
                        (current.newest_loaded_message_id IS NULL OR
                         current.newest_loaded_message_id < prior.newest_loaded_message_id))
                    OR
                    (prior.history_complete <> 0 AND current.history_complete = 0)
                )
                AND (
                    current.oldest_loaded_message_id IS NOT prior.oldest_loaded_message_id
                    OR current.newest_loaded_message_id IS NOT prior.newest_loaded_message_id
                    OR current.history_complete IS NOT prior.history_complete
                )
                THEN 1 ELSE 0 END), 0)
        FROM acceptance_snapshot.cursors prior
        JOIN chat_sync_state current
          ON current.account_id=prior.account_id
         AND current.namespace_version=prior.namespace_version
         AND current.chat_id=prior.chat_id
        """
    ).fetchone()
    return CursorComparison(
        before_count=before_count,
        after_count=after_count,
        missing_count=missing_count,
        regressed_count=regressed_count,
        progressed_count=progressed_count,
    )


def snapshots_have_equal_items(
    first: Path, second: Path, deadline: Deadline | None = None
) -> bool:
    db = sqlite3.connect(f"file:{first}?mode=ro", uri=True)
    if deadline is not None:
        db.set_progress_handler(deadline.sqlite_progress, 10_000)
    try:
        db.execute(
            "ATTACH DATABASE ? AS candidate",
            (f"file:{second}?mode=ro",),
        )
        first_count = db.execute("SELECT count(*) FROM active_items").fetchone()[0]
        second_count = db.execute(
            "SELECT count(*) FROM candidate.active_items"
        ).fetchone()[0]
        if first_count != second_count:
            return False
        missing = db.execute(
            """
            SELECT 1 FROM active_items prior
            WHERE NOT EXISTS (
                SELECT 1 FROM candidate.active_items current
                WHERE current.item_id=prior.item_id
            ) LIMIT 1
            """
        ).fetchone()
        return missing is None
    finally:
        db.close()


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
            old_oldest is None or (new_oldest is not None and new_oldest <= old_oldest)
        ) and (
            old_newest is None or (new_newest is not None and new_newest >= old_newest)
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


def write_private_json(path: Path, value: dict) -> None:
    """Atomically write private identifiers with owner-only permissions."""
    path.parent.mkdir(parents=True, exist_ok=True)
    writing = path.with_name(f"{path.name}.writing")
    writing.unlink(missing_ok=True)
    descriptor = os.open(writing, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as destination:
            destination.write(json.dumps(value, indent=2, sort_keys=True) + "\n")
            destination.flush()
            os.fsync(destination.fileno())
    except BaseException:
        writing.unlink(missing_ok=True)
        raise
    os.replace(writing, path)
    os.chmod(path, 0o600)


def validate_public_evidence(evidence: dict) -> None:
    unexpected = set(evidence) - PUBLIC_EVIDENCE_FIELDS
    if unexpected:
        raise AcceptanceFailure("public-evidence-field-not-allow-listed")
    if evidence.get("privacy_safe") is not True:
        raise AcceptanceFailure("public-evidence-not-marked-privacy-safe")
    phase = evidence.get("phase")
    timeout_stage = evidence.get("timeout_stage")
    if phase is not None and phase not in PHASE_STAGES:
        raise AcceptanceFailure("public-phase-invalid")
    if timeout_stage is not None and (
        phase not in PHASE_STAGES or timeout_stage not in ("none", *PHASE_STAGES[phase])
    ):
        raise AcceptanceFailure("public-timeout-stage-invalid")
    for key, value in evidence.items():
        if key == "stage_timings_ms":
            if (
                phase not in PHASE_STAGES
                or not isinstance(value, dict)
                or set(value) != set(PHASE_STAGES[phase])
                or any(
                    timing is not None
                    and (
                        isinstance(timing, bool)
                        or not isinstance(timing, int)
                        or timing < 0
                    )
                    for timing in value.values()
                )
            ):
                raise AcceptanceFailure("public-stage-timings-invalid")
            continue
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
    dataless_probe: Callable[
        [Path], bool | PlaceholderProbeResult
    ] = finder_placeholder_probe,
    deadline: Deadline | None = None,
    recorder: StageRecorder | None = None,
) -> dict:
    started = time.monotonic()
    deadline = deadline or Deadline(None)
    recorder = recorder or StageRecorder("before", deadline)
    with recorder.stage("select_candidate"):
        db = connection(database, deadline)
        try:
            candidate, attachment, selection_facts = select_uncached_dataless_candidate(
                db, cloud_root, dataless_probe
            )
            ancestor_ids = (candidate.list_id, candidate.chat_id, candidate.month_id)
            ancestor_paths = [cloud_root]
            ancestor_paths.extend(
                item_path(db, cloud_root, item_id) for item_id in ancestor_ids
            )
            markdown = item_path(db, cloud_root, candidate.markdown_id)
            ndjson = item_path(db, cloud_root, candidate.ndjson_id)
            pre_uncached = not cache_verified(db, candidate.item_id)
            pre_dataless = require_dataless_probe(attachment, dataless_probe)
        finally:
            db.close()
        if not pre_uncached or not pre_dataless:
            raise AcceptanceFailure("placeholder-precondition-changed")

    with recorder.stage("enumerate_ancestors"):
        for path in ancestor_paths:
            deadline.check()
            with os.scandir(path) as entries:
                tuple(entries)
        after_enumeration = connection(database, deadline)
        try:
            post_uncached = not cache_verified(after_enumeration, candidate.item_id)
            post_dataless = require_dataless_probe(attachment, dataless_probe)
        finally:
            after_enumeration.close()
        if not post_uncached or not post_dataless:
            raise AcceptanceFailure("enumeration-materialized-sampled-placeholder")

    with recorder.stage("verify_generated_documents"):
        generated_db = connection(database, deadline)
        try:
            generated_records = verify_generated_documents(
                generated_db,
                cloud_root,
                (candidate.markdown_id, candidate.ndjson_id, candidate.chat_json_id),
            )
        finally:
            generated_db.close()
        markdown_size = next(
            record["logical_size"]
            for record in generated_records
            if record["mime_type"] == "text/markdown"
        )
        ndjson_size = next(
            record["logical_size"]
            for record in generated_records
            if record["mime_type"] == "application/x-ndjson"
        )

    with recorder.stage("verify_generated_storage"):
        storage_db = connection(database, deadline)
        try:
            generated_storage = verify_generated_storage(
                storage_db, data_root, deadline
            )
        finally:
            storage_db.close()

    with recorder.stage("hydrate_attachment"):
        hydration_started = time.monotonic()
        hydrated_digest, byte_count = read_placeholder_once(attachment)
        hydration_ms = round((time.monotonic() - hydration_started) * 1000)

    with recorder.stage("publish_hydration"):
        blob = None
        for _ in range(HYDRATION_WAIT_ATTEMPTS):
            deadline.check()
            check = connection(database, deadline)
            try:
                blob = check.execute(
                    "SELECT blob_hash, size FROM cache_entries "
                    "WHERE item_id=? AND verification='verified'",
                    (candidate.item_id,),
                ).fetchone()
            finally:
                check.close()
            if blob is not None:
                break
            time.sleep(
                min(
                    HYDRATION_WAIT_SECONDS,
                    max(0, deadline.remaining_ms() or 100) / 1000,
                )
            )
        if blob is None:
            raise AcceptanceFailure("hydration-cache-publication-timeout")

    with recorder.stage("snapshot_identity_and_cursors"):
        indexed_counts = create_indexed_snapshot(
            database, snapshot_database_path(state_path), deadline
        )
        current = connection(database, deadline)
        try:
            snapshot = scalar_aggregate(current)
        finally:
            current.close()
        if indexed_counts != {
            "item_count": snapshot["item_count"],
            "cursor_count": snapshot["cursor_count"],
        }:
            raise AcceptanceFailure("private-snapshot-count-mismatch")

    with recorder.stage("verify_namespace"):
        current = connection(database, deadline)
        try:
            namespace = namespace_facts(current)
        finally:
            current.close()

    private_state = {
        **snapshot,
        "snapshot_schema_version": SNAPSHOT_SCHEMA_VERSION,
        "sample_item": candidate.item_id.hex(),
        "expected_size": candidate.expected_size,
        "hydrated_digest": hydrated_digest,
        "generated_records": generated_records,
    }
    with recorder.stage("write_evidence"):
        write_private_json(state_path, private_state)
        evidence = {
            "privacy_safe": True,
            **selection_facts.public_evidence(),
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
            "generated_scan_entry_count": generated_storage.scan_entry_count,
            "generated_scan_entry_limit": MAX_GENERATED_SCAN_ENTRIES,
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
    deadline: Deadline | None = None,
    recorder: StageRecorder | None = None,
) -> dict:
    started = time.monotonic()
    deadline = deadline or Deadline(None)
    recorder = recorder or StageRecorder("after", deadline)
    with recorder.stage("load_private_state"):
        previous = json.loads(state_path.read_text())
        if previous.get("snapshot_schema_version") != SNAPSHOT_SCHEMA_VERSION:
            raise AcceptanceFailure("private-snapshot-schema-invalid")
        prior_generated = previous["generated_records"]
        sample_item = bytes.fromhex(previous["sample_item"])
        evidence = json.loads(evidence_path.read_text())

    with recorder.stage("snapshot_current_state"):
        db = connection(database, deadline)
        try:
            current = scalar_aggregate(db)
        finally:
            db.close()

    with recorder.stage("verify_hydration"):
        db = connection(database, deadline)
        try:
            blob = db.execute(
                "SELECT blob_hash, size FROM cache_entries "
                "WHERE item_id=? AND verification='verified'",
                (sample_item,),
            ).fetchone()
        finally:
            db.close()

    with recorder.stage("verify_generated_documents"):
        db = connection(database, deadline)
        try:
            current_generated = verify_generated_documents(
                db,
                cloud_root,
                tuple(bytes.fromhex(record["item_id"]) for record in prior_generated),
            )
        finally:
            db.close()

    with recorder.stage("verify_generated_storage"):
        db = connection(database, deadline)
        try:
            generated_storage = verify_generated_storage(db, data_root, deadline)
        finally:
            db.close()

    snapshot_path = snapshot_database_path(state_path)
    comparison_db = connection(database, deadline)
    try:
        attach_snapshot(comparison_db, snapshot_path)
        with recorder.stage("compare_identity"):
            items = compare_items_indexed(comparison_db)
            sampled_identity_stable = (
                comparison_db.execute(
                    "SELECT count(*) FROM items "
                    "WHERE item_id=? AND deleted_at_ms IS NULL",
                    (sample_item,),
                ).fetchone()[0]
                == 1
            )
        with recorder.stage("compare_cursors"):
            cursors = compare_cursors_indexed(comparison_db)
    finally:
        comparison_db.close()

    with recorder.stage("verify_namespace"):
        db = connection(database, deadline)
        try:
            namespace = namespace_facts(db)
        finally:
            db.close()

    with recorder.stage("write_evidence"):
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
                "generated_scan_entry_count": generated_storage.scan_entry_count,
                "generated_scan_entry_limit": MAX_GENERATED_SCAN_ENTRIES,
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
                "story_transition_observed": current["stories"] != previous["stories"],
                "chat_json_present": namespace.hidden_metadata_complete,
                "active_stories_present": namespace.active_story_container_count > 0,
                "hidden_chat_metadata_complete": namespace.hidden_metadata_complete,
                "legacy_chat_metadata_absent": namespace.legacy_metadata_count == 0,
                "zero_story_chat_count": namespace.zero_story_chat_count,
                "zero_story_containers_omitted": namespace.zero_story_containers_omitted,
                "nonempty_story_chat_count": namespace.nonempty_story_chat_count,
                "story_containers_truthful": namespace.story_containers_truthful,
                "elapsed_ms": round((time.monotonic() - started) * 1000),
            }
        )
        evidence["before_item_count"] = items.before_count
        evidence["before_cursor_count"] = cursors.before_count
        evidence["story_count_before"] = previous["stories"][0]
        write_json(evidence_path, evidence)
    return evidence


def run_stability_snapshot(
    database: Path,
    state_path: Path,
    evidence_path: Path,
    deadline: Deadline | None = None,
    recorder: StageRecorder | None = None,
) -> dict:
    """Snapshot a quiescent item set around the already allowed hydration."""
    deadline = deadline or Deadline(None)
    recorder = recorder or StageRecorder("stability-snapshot", deadline)
    with recorder.stage("load_private_state"):
        previous = json.loads(state_path.read_text())

    with recorder.stage("verify_persisted_sample"):
        db = connection(database, deadline)
        try:
            verify_persisted_sample(db, previous)
        finally:
            db.close()

    with recorder.stage("snapshot_identity_and_cursors"):
        snapshot_path = snapshot_database_path(state_path)
        poll_paths = (
            snapshot_path.with_name(f"{snapshot_path.name}.poll-a"),
            snapshot_path.with_name(f"{snapshot_path.name}.poll-b"),
        )
        for path in poll_paths:
            path.unlink(missing_ok=True)
        stable_polls = 0
        previous_poll: Path | None = None
        current_poll: Path | None = None
        indexed_counts = None
        for attempt in range(QUIESCENCE_ATTEMPTS):
            deadline.check()
            current_poll = poll_paths[attempt % 2]
            current_poll.unlink(missing_ok=True)
            db = connection(database, deadline)
            try:
                verify_persisted_sample(db, previous)
            finally:
                db.close()
            indexed_counts = create_indexed_snapshot(database, current_poll, deadline)
            equal = previous_poll is not None and snapshots_have_equal_items(
                previous_poll, current_poll, deadline
            )
            stable_polls = stable_polls + 1 if equal else 1
            if stable_polls >= QUIESCENCE_STABLE_POLLS:
                break
            previous_poll = current_poll
            time.sleep(QUIESCENCE_WAIT_SECONDS)
        else:
            raise AcceptanceFailure("active-item-set-did-not-quiesce")
        if current_poll is None or indexed_counts is None:
            raise AcceptanceFailure("quiescent-snapshot-not-found")
        os.replace(current_poll, snapshot_path)
        for path in poll_paths:
            path.unlink(missing_ok=True)
        db = connection(database, deadline)
        try:
            snapshot = scalar_aggregate(db)
        finally:
            db.close()
        if indexed_counts != {
            "item_count": snapshot["item_count"],
            "cursor_count": snapshot["cursor_count"],
        }:
            raise AcceptanceFailure("private-snapshot-count-mismatch")

    with recorder.stage("write_evidence"):
        write_private_json(
            state_path,
            {
                **snapshot,
                "snapshot_schema_version": SNAPSHOT_SCHEMA_VERSION,
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
    if evidence.get("failure_category", "none") != "none":
        return False
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
    parser.add_argument(
        "--deadline-seconds",
        type=float,
        default=DEFAULT_OVERALL_DEADLINE_SECONDS,
        help="hard wall-clock bound for the entire phase",
    )
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--progress", type=Path, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


@dataclass(frozen=True)
class WorkerProcessResult:
    returncode: int
    stdout: str
    stderr: str
    timed_out: bool
    cleanup_complete: bool
    elapsed_ms: int


def _remaining_seconds(deadline: float) -> float:
    return max(0.0, deadline - time.monotonic())


def worker_cleanup_reserve(timeout: float) -> float:
    """Reserve enough of short deadlines for TERM, KILL, and bounded reaping."""
    return min(WORKER_CLEANUP_RESERVE_SECONDS, timeout / 2)


def terminate_process_group(
    process: subprocess.Popen[str], cleanup_deadline: float
) -> bool:
    """Terminate one worker group and reap its leader within an absolute bound."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    remaining = _remaining_seconds(cleanup_deadline)
    term_grace = min(WORKER_TERM_GRACE_SECONDS, remaining / 2)
    if process.poll() is None and term_grace > 0:
        try:
            process.wait(timeout=term_grace)
        except subprocess.TimeoutExpired:
            pass

    # Kill the exact group even if its leader accepted TERM: descendants may
    # still hold inherited pipes or continue provider/stat work.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        try:
            process.wait(timeout=_remaining_seconds(cleanup_deadline))
        except subprocess.TimeoutExpired:
            return False
    return process.poll() is not None


def collect_worker_output(
    process: subprocess.Popen[str], deadline: float
) -> tuple[str, str]:
    """Drain killed-worker pipes only while the caller's deadline remains."""
    try:
        return process.communicate(timeout=_remaining_seconds(deadline))
    except subprocess.TimeoutExpired:
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()
        return "", ""


def run_worker_process(command: Sequence[str], timeout: float) -> WorkerProcessResult:
    started = time.monotonic()
    overall_deadline = started + timeout
    cleanup_reserve = worker_cleanup_reserve(timeout)
    execution_timeout = max(0.001, timeout - cleanup_reserve)
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=execution_timeout)
        return WorkerProcessResult(
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
            timed_out=False,
            cleanup_complete=process.poll() is not None,
            elapsed_ms=round((time.monotonic() - started) * 1000),
        )
    except subprocess.TimeoutExpired:
        cleanup_complete = terminate_process_group(process, overall_deadline)
        stdout, stderr = collect_worker_output(process, overall_deadline)
        return WorkerProcessResult(
            returncode=process.returncode if process.returncode is not None else -1,
            stdout=stdout,
            stderr=stderr,
            timed_out=True,
            cleanup_complete=cleanup_complete,
            elapsed_ms=round((time.monotonic() - started) * 1000),
        )


def failure_evidence(
    phase: str,
    deadline_ms: int,
    elapsed_ms: int,
    category: str,
    timeout_stage: str,
    timings: dict[str, int | None],
) -> dict:
    return {
        "privacy_safe": True,
        "phase": phase,
        "deadline_ms": deadline_ms,
        "deadline_remaining_ms": 0,
        "elapsed_ms": elapsed_ms,
        "failure_category": category,
        "timeout_stage": timeout_stage,
        "stage_timings_ms": {
            stage: timings.get(stage) for stage in PHASE_STAGES[phase]
        },
    }


def run_worker(args: argparse.Namespace) -> int:
    budget = max(0.001, args.deadline_seconds)
    deadline = Deadline(budget)
    recorder = StageRecorder(args.phase, deadline, args.progress)
    database = args.data_root / "state/gramdrive.sqlite3"
    try:
        if args.phase == "before":
            evidence = run_before(
                database,
                args.data_root,
                args.cloud_root,
                args.state,
                args.evidence,
                deadline=deadline,
                recorder=recorder,
            )
        elif args.phase == "stability-snapshot":
            evidence = run_stability_snapshot(
                database,
                args.state,
                args.evidence,
                deadline=deadline,
                recorder=recorder,
            )
        else:
            evidence = run_after(
                database,
                args.data_root,
                args.cloud_root,
                args.state,
                args.evidence,
                deadline=deadline,
                recorder=recorder,
            )
        recorder.decorate(evidence, round(args.deadline_seconds * 1000))
        write_json(args.evidence, evidence)
    except (
        AcceptanceFailure,
        AttributeError,
        OSError,
        sqlite3.Error,
        TypeError,
        ValueError,
        KeyError,
    ) as error:
        if isinstance(error, AcceptanceFailure):
            label = str(error)
        elif deadline.remaining_ms() == 0:
            label = "overall-deadline-exceeded"
        else:
            label = "acceptance-io-failed"
        evidence = failure_evidence(
            args.phase,
            round(args.deadline_seconds * 1000),
            round((time.monotonic() - deadline.started) * 1000),
            label,
            recorder.failed_stage or "none",
            recorder.timings,
        )
        if isinstance(error, AcceptanceFailure):
            evidence.update(error.public_evidence)
        write_json(args.evidence, evidence)
        print(f"installed live-content acceptance failed: {label}", file=sys.stderr)
        return 1
    passed = evidence_passed(args.phase, evidence)
    if not passed:
        evidence["failure_category"] = "acceptance-assertion-failed"
        write_json(args.evidence, evidence)
    try:
        validate_public_evidence(evidence)
    except AcceptanceFailure as error:
        print(f"installed live-content acceptance failed: {error}", file=sys.stderr)
        return 1
    return 0 if passed else 1


def worker_command(
    args: argparse.Namespace, progress: Path, budget: float
) -> list[str]:
    return [
        sys.executable,
        str(Path(__file__).resolve()),
        args.phase,
        "--data-root",
        str(args.data_root),
        "--cloud-root",
        str(args.cloud_root),
        "--state",
        str(args.state),
        "--evidence",
        str(args.evidence),
        "--deadline-seconds",
        str(budget),
        "--progress",
        str(progress),
        "--worker",
    ]


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.deadline_seconds <= 0:
        print(
            "installed live-content acceptance failed: invalid-deadline",
            file=sys.stderr,
        )
        return 2
    if args.worker:
        return run_worker(args)

    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    progress = args.evidence.with_name(f"{args.evidence.name}.progress.json")
    progress.unlink(missing_ok=True)
    reserve = worker_cleanup_reserve(args.deadline_seconds)
    worker_budget = max(0.001, args.deadline_seconds - reserve)
    result = run_worker_process(
        worker_command(args, progress, worker_budget), args.deadline_seconds
    )
    evidence = None
    try:
        evidence = json.loads(args.evidence.read_text())
    except (OSError, json.JSONDecodeError):
        pass
    if result.timed_out:
        timings: dict[str, int | None] = {}
        timeout_stage = "none"
        try:
            progress_record = json.loads(progress.read_text())
            timings = progress_record.get("stage_timings_ms", {})
            timeout_stage = progress_record.get("current_stage") or "none"
            current_elapsed = progress_record.get("current_stage_elapsed_ms")
            current_started_ns = progress_record.get(
                "current_stage_started_monotonic_ns"
            )
            if timeout_stage in PHASE_STAGES[args.phase]:
                if isinstance(current_started_ns, int):
                    current_elapsed = min(
                        result.elapsed_ms,
                        max(
                            0,
                            round(
                                (time.monotonic_ns() - current_started_ns) / 1_000_000
                            ),
                        ),
                    )
                if isinstance(current_elapsed, int):
                    timings[timeout_stage] = current_elapsed
        except (OSError, json.JSONDecodeError, AttributeError):
            pass
        evidence = failure_evidence(
            args.phase,
            round(args.deadline_seconds * 1000),
            result.elapsed_ms,
            (
                "overall-deadline-exceeded"
                if result.cleanup_complete
                else "worker-cleanup-deadline-exceeded"
            ),
            timeout_stage,
            timings,
        )
    if not isinstance(evidence, dict):
        evidence = failure_evidence(
            args.phase,
            round(args.deadline_seconds * 1000),
            result.elapsed_ms,
            "worker-evidence-missing",
            "none",
            {},
        )
    evidence["deadline_ms"] = round(args.deadline_seconds * 1000)
    evidence["deadline_remaining_ms"] = max(
        0, evidence["deadline_ms"] - result.elapsed_ms
    )
    evidence["child_cleanup_complete"] = result.cleanup_complete
    evidence["worker_exit_code"] = result.returncode
    evidence["elapsed_ms"] = result.elapsed_ms
    write_json(args.evidence, evidence)
    progress.unlink(missing_ok=True)
    cleanup_incomplete_private_artifacts(args.state)
    try:
        validate_public_evidence(evidence)
    except AcceptanceFailure as error:
        print(f"installed live-content acceptance failed: {error}", file=sys.stderr)
        return 1
    if result.stderr:
        print(result.stderr.rstrip(), file=sys.stderr)
    print(json.dumps(evidence, sort_keys=True))
    if result.returncode != 0 or result.timed_out:
        return 1
    return 0 if evidence_passed(args.phase, evidence) else 1


if __name__ == "__main__":
    raise SystemExit(main())
