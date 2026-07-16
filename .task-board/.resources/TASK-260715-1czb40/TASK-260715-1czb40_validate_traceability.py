#!/usr/bin/env python3
"""Validate the requirement coverage matrix (docs/TRACEABILITY.md).

Checks, all fatal (exit code 1):
  1. Every requirement ID defined in .spec/ has exactly one matrix row.
  2. Every matrix row references a requirement ID that exists in .spec/.
  3. Every board element ID referenced in the matrix exists in .task-board/.
  4. Every row maps to at least one board element unless its disposition is
     'future' (explicitly uncommitted scope).
  5. Rows mapping to multiple board elements carry a non-empty justification
     in Notes.
  6. Rows use a known disposition value.
  7. Rows with disposition 'active' reference at least one board element
     outside the deferred epics declared in the matrix header.
  8. Every requirement-shaped ID mentioned in board READMEs exists in .spec/
     (no orphan/stale requirement references on the board).

Usage: python3 .scripts/validate_traceability.py [--repo-root PATH]
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import Counter
from pathlib import Path

MATRIX_PATH = Path("docs/TRACEABILITY.md")
SPEC_DIR = Path(".spec")
BOARD_DIR = Path(".task-board")

ALLOWED_DISPOSITIONS = {"active", "deferred-platform", "deferred-optional", "future"}
UNMAPPED_ALLOWED_DISPOSITIONS = {"future"}

BOARD_ID_RE = re.compile(r"\b(?:EPIC|STORY|TASK|BUG)-\d{6}-[a-z0-9]{6}\b")
BOARD_DIR_RE = re.compile(r"^((?:EPIC|STORY|TASK|BUG)-\d{6}-[a-z0-9]{6})(?:_|$)")
REQ_ID_RE = re.compile(
    r"\b(?:"
    r"(?:PRD|DOM|SYNC|NFR|SEC)-\d{3}"
    r"|PLAT-(?:MAC|IOS|WIN|AND|LNX)-\d{3}"
    r"|PLAT-\d{3}"
    r"|DEC-\d{3}"
    r"|POL-[1-9]\d?"
    r")\b"
)

# Requirement definitions in .spec/:
#   - bullet definitions:  - **PRD-001 (V1):** text   /  - **DOM-001:** text
#   - decision table rows: | **DEC-001** | Accepted | ...
#   - policy headings:     ## POL-1. Title (DEC-013)
BULLET_DEF_RE = re.compile(
    r"^\s*-\s+\*\*((?:PRD|DOM|SYNC|SEC|NFR)-\d{3}|PLAT-(?:MAC|IOS|WIN|AND|LNX)-\d{3}|PLAT-\d{3})"
    r"(?:\s*\(([^)]*)\))?\s*:?\*\*"
)
DEC_ROW_RE = re.compile(r"^\|\s*\*\*(DEC-\d{3})\*\*\s*\|")
POL_HEAD_RE = re.compile(r"^##\s+(POL-[1-9]\d?)\.")

MATRIX_ROW_RE = re.compile(r"^\|\s*((?:PRD|DOM|SYNC|PLAT|SEC|NFR|DEC|POL)-[A-Z0-9-]+)\s*\|")
DEFERRED_EPICS_RE = re.compile(r"<!--\s*deferred-epics:\s*([^>]*?)\s*-->")


def collect_spec_ids(spec_dir: Path) -> dict[str, str]:
    """Return {requirement_id: defining_file}."""
    defined: dict[str, str] = {}
    for path in sorted(spec_dir.glob("*.md")):
        for line in path.read_text(encoding="utf-8").splitlines():
            for regex in (BULLET_DEF_RE, DEC_ROW_RE, POL_HEAD_RE):
                m = regex.match(line)
                if m:
                    defined[m.group(1)] = path.name
                    break
    return defined


def collect_board_ids(board_dir: Path) -> dict[str, str]:
    """Return {element_id: top_level_epic_id} from board directory names."""
    elements: dict[str, str] = {}
    for path in board_dir.rglob("*"):
        if not path.is_dir() or ".resources" in path.parts:
            continue
        m = BOARD_DIR_RE.match(path.name)
        if not m:
            continue
        rel = path.relative_to(board_dir)
        epic_match = BOARD_DIR_RE.match(rel.parts[0])
        elements[m.group(1)] = epic_match.group(1) if epic_match else ""
    return elements


def parse_matrix(matrix_path: Path):
    """Return (rows, deferred_epics). rows = list of dicts."""
    text = matrix_path.read_text(encoding="utf-8")
    deferred: set[str] = set()
    m = DEFERRED_EPICS_RE.search(text)
    if m:
        deferred = set(m.group(1).split())

    rows = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        row_match = MATRIX_ROW_RE.match(line)
        if not row_match:
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 5:
            rows.append({"id": row_match.group(1), "line": lineno, "malformed": True})
            continue
        req_id, tier, disposition, elements_cell, notes = cells[:5]
        rows.append(
            {
                "id": req_id,
                "line": lineno,
                "malformed": False,
                "tier": tier,
                "disposition": disposition,
                "elements": BOARD_ID_RE.findall(elements_cell),
                "notes": notes,
            }
        )
    return rows, deferred


def scan_board_requirement_refs(board_dir: Path) -> dict[str, set[str]]:
    """Return {requirement_id: {relative_readme_paths}} for board README mentions."""
    refs: dict[str, set[str]] = {}
    for readme in board_dir.rglob("README.md"):
        if ".resources" in readme.parts:
            continue
        text = readme.read_text(encoding="utf-8")
        for req in REQ_ID_RE.findall(text):
            refs.setdefault(req, set()).add(str(readme.relative_to(board_dir)))
    return refs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", default=".", help="repository root (default: cwd)")
    args = parser.parse_args()
    root = Path(args.repo_root).resolve()

    matrix_path = root / MATRIX_PATH
    spec_dir = root / SPEC_DIR
    board_dir = root / BOARD_DIR
    for required in (matrix_path, spec_dir, board_dir):
        if not required.exists():
            print(f"ERROR: missing required path: {required}", file=sys.stderr)
            return 1

    spec_ids = collect_spec_ids(spec_dir)
    board_ids = collect_board_ids(board_dir)
    rows, deferred_epics = parse_matrix(matrix_path)

    errors: list[str] = []

    if not deferred_epics:
        errors.append("matrix header: missing '<!-- deferred-epics: ... -->' declaration")
    for epic in sorted(deferred_epics):
        if epic not in board_ids:
            errors.append(f"matrix header: deferred epic {epic} does not exist on the board")

    row_ids = Counter(r["id"] for r in rows)
    for req_id, count in sorted(row_ids.items()):
        if count > 1:
            errors.append(f"duplicate matrix rows for {req_id} ({count} rows)")

    for req_id, source in sorted(spec_ids.items()):
        if req_id not in row_ids:
            errors.append(f"missing matrix row for {req_id} (defined in {source})")

    for row in rows:
        rid, line = row["id"], row["line"]
        if rid not in spec_ids:
            errors.append(f"line {line}: matrix row {rid} has no definition in .spec/")
        if row.get("malformed"):
            errors.append(f"line {line}: malformed matrix row for {rid}")
            continue
        disposition = row["disposition"]
        if disposition not in ALLOWED_DISPOSITIONS:
            errors.append(f"line {line}: {rid} has unknown disposition '{disposition}'")
        elements = row["elements"]
        if not elements and disposition not in UNMAPPED_ALLOWED_DISPOSITIONS:
            errors.append(f"line {line}: {rid} maps to no board element and is not 'future'")
        for element in elements:
            if element not in board_ids:
                errors.append(f"line {line}: {rid} references orphan board element {element}")
        if len(elements) > 1 and not row["notes"]:
            errors.append(f"line {line}: {rid} has multiple mappings without justification notes")
        if disposition == "active" and elements:
            known = [e for e in elements if e in board_ids]
            if known and not any(board_ids[e] not in deferred_epics for e in known):
                errors.append(
                    f"line {line}: {rid} is 'active' but maps only into deferred epics"
                )

    board_refs = scan_board_requirement_refs(board_dir)
    for req_id, files in sorted(board_refs.items()):
        if req_id not in spec_ids:
            listing = ", ".join(sorted(files))
            errors.append(f"board references undefined requirement {req_id} in: {listing}")

    if errors:
        print(f"FAIL: {len(errors)} traceability error(s):", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    dispositions = Counter(r["disposition"] for r in rows)
    print(
        "OK: {total} requirements from .spec/ all mapped exactly once "
        "({active} active, {dp} deferred-platform, {do} deferred-optional, {future} future); "
        "{elements} board elements referenced; no orphan references on the board.".format(
            total=len(rows),
            active=dispositions.get("active", 0),
            dp=dispositions.get("deferred-platform", 0),
            do=dispositions.get("deferred-optional", 0),
            future=dispositions.get("future", 0),
            elements=len({e for r in rows if not r.get("malformed") for e in r["elements"]}),
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
