#!/usr/bin/env python3
"""Select authenticated stable Release state without network access."""
from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import sys
from collections.abc import Sequence
from pathlib import Path


SEMVER_RE = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
)
RFC3339_RE = re.compile(
    r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"
    r"(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:[0-9]{2})"
)
CANDIDATE_ASSETS = (
    "stable-pages-site-manifest.json",
    "stable-pages-site.attestation.json",
)
PRIOR_SITE_ASSETS = (
    "stable-pages-site.tar.gz",
    "stable-pages-site-manifest.json",
    "stable-pages-site-manifest.signature.txt",
    "stable-pages-site.attestation.json",
)


class ReleaseSelectionError(ValueError):
    """The GitHub Release response cannot establish one safe state head."""


def parse_semver(tag: str) -> tuple[int, int, int] | None:
    match = SEMVER_RE.fullmatch(tag)
    if match is None:
        return None
    return tuple(int(value) for value in match.groups())


def flatten_release_pages(payload: object) -> list[object]:
    """Flatten the exact outer-page array emitted by `gh api --paginate --slurp`."""
    if not isinstance(payload, list) or not payload:
        raise ReleaseSelectionError("paginated GitHub Releases response must contain pages")
    releases: list[object] = []
    for index, page in enumerate(payload, start=1):
        if not isinstance(page, list):
            raise ReleaseSelectionError(
                f"paginated GitHub Releases page {index} must be an array"
            )
        releases.extend(page)
    return releases


def validate_published_at(value: object, tag: str) -> None:
    if not isinstance(value, str) or RFC3339_RE.fullmatch(value) is None:
        raise ReleaseSelectionError(f"GitHub Release {tag} has invalid publication time")
    try:
        parsed = dt.datetime.fromisoformat(value[:-1] + "+00:00" if value.endswith("Z") else value)
        offset = parsed.utcoffset()
    except ValueError as error:
        raise ReleaseSelectionError(
            f"GitHub Release {tag} has invalid publication time"
        ) from error
    if offset is None:
        raise ReleaseSelectionError(f"GitHub Release {tag} has invalid publication time")


def published_stable_releases(payload: object) -> list[tuple[tuple[int, int, int], str, list[str]]]:
    if not isinstance(payload, list):
        raise ReleaseSelectionError("GitHub Releases response must be an array")
    releases: list[tuple[tuple[int, int, int], str, list[str]]] = []
    seen_versions: set[tuple[int, int, int]] = set()
    for release in payload:
        if not isinstance(release, dict):
            raise ReleaseSelectionError("GitHub Release record must be an object")
        tag = release.get("tag_name")
        draft = release.get("draft")
        prerelease = release.get("prerelease")
        published_at = release.get("published_at")
        if not isinstance(tag, str) or type(draft) is not bool or type(prerelease) is not bool:
            raise ReleaseSelectionError("GitHub Release identity fields are malformed")
        version = parse_semver(tag)
        if published_at is None:
            if not draft:
                raise ReleaseSelectionError(f"GitHub Release {tag} has invalid publication time")
        else:
            validate_published_at(published_at, tag)
        assets = release.get("assets")
        if not isinstance(assets, list):
            raise ReleaseSelectionError(f"GitHub Release {tag} has malformed assets")
        asset_names: list[str] = []
        for asset in assets:
            if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
                raise ReleaseSelectionError(f"GitHub Release {tag} has malformed asset")
            asset_names.append(asset["name"])
        if draft or prerelease or version is None:
            continue
        if version in seen_versions:
            raise ReleaseSelectionError(f"published stable semver is duplicated: {tag}")
        seen_versions.add(version)
        releases.append((version, tag, asset_names))
    return releases


def require_singular_assets(tag: str, assets: list[str], required: Sequence[str]) -> None:
    for name in required:
        count = assets.count(name)
        if count != 1:
            raise ReleaseSelectionError(
                f"published stable Release {tag} must contain exactly one {name}; found {count}"
            )


def select_candidate_state_head(payload: object) -> str | None:
    """Return the newest stable semver; its build-floor evidence must be complete."""
    releases = published_stable_releases(payload)
    if not releases:
        return None
    _, tag, assets = max(releases, key=lambda release: release[0])
    require_singular_assets(tag, assets, CANDIDATE_ASSETS)
    return tag


def select_prior_site(payload: object, current_tag: str) -> str | None:
    """Return the newest complete prior site while allowing current-tag recovery."""
    current_version = parse_semver(current_tag)
    if current_version is None:
        raise ReleaseSelectionError(f"current source tag is not exact stable semver: {current_tag}")
    releases = published_stable_releases(payload)
    newer = [tag for version, tag, _ in releases if version > current_version]
    if newer:
        raise ReleaseSelectionError(
            f"cannot restore {current_tag} with newer published stable state: "
            f"{max(newer, key=lambda tag: parse_semver(tag) or (0, 0, 0))}"
        )
    prior = [release for release in releases if release[0] < current_version]
    if not prior:
        return None
    _, tag, assets = max(prior, key=lambda release: release[0])
    require_singular_assets(tag, assets, PRIOR_SITE_ASSETS)
    return tag


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-pages", type=Path, required=True)
    parser.add_argument(
        "--mode", choices=("candidate-state-head", "stable-prior"), required=True
    )
    parser.add_argument("--current-tag")
    args = parser.parse_args(argv)
    if args.mode == "stable-prior" and args.current_tag is None:
        parser.error("--current-tag is required for stable-prior")
    if args.mode == "candidate-state-head" and args.current_tag is not None:
        parser.error("--current-tag is only valid for stable-prior")
    try:
        pages = json.loads(args.release_pages.read_text(encoding="utf-8"))
        payload = flatten_release_pages(pages)
        if args.mode == "candidate-state-head":
            selected = select_candidate_state_head(payload)
        else:
            selected = select_prior_site(payload, args.current_tag)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ReleaseSelectionError) as error:
        print(f"STABLE RELEASE SELECTION FAILED: {error}", file=sys.stderr)
        return 1
    if selected is not None:
        print(selected)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
