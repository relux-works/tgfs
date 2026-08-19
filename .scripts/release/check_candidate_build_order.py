#!/usr/bin/env python3
"""Resolve the highest published Sparkle build without mutating a feed."""
from __future__ import annotations

import argparse
import base64
import binascii
import json
import os
import re
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path, PurePosixPath

TEST_FEED = "https://github.com/relux-works/tgfs/releases/download/updates-test-v1/test.xml"
STABLE_FEED_TEMPLATE = "https://relux-works.github.io/tgfs/updates/stable/v{generation}/stable.xml"
SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
STABLE_CONFIG = Path(__file__).resolve().parents[2] / ".github/sparkle-stable.json"
Fetch = Callable[[str], bytes | None]
SHA256_RE = re.compile(r"[0-9a-f]{64}")


@dataclass(frozen=True)
class FeedEndpoint:
    url: str
    expected_sha256: str | None = None
    expected_bytes: int | None = None
    absent_only: bool = False


def fetch(url: str) -> bytes | None:
    request = urllib.request.Request(url, headers={"User-Agent": "GramDrive-candidate-build/1"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read(4 * 1024 * 1024)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def builds(xml: bytes) -> list[int]:
    root = ET.fromstring(xml)
    if root.tag != "rss":
        raise ValueError("feed root must be an unnamespaced rss element")
    channels = [element for element in root if element.tag == "channel"]
    if len(channels) != 1:
        raise ValueError("feed must have exactly one direct unnamespaced channel")
    result: list[int] = []
    version_name = f"{{{SPARKLE_NS}}}version"
    channel = channels[0]
    items = [element for element in channel if element.tag == "item"]
    for item in items:
        values: list[str] = []
        for child in item:
            if child.tag == version_name:
                if child.attrib or len(child):
                    raise ValueError("Sparkle version must be an attribute-free leaf element")
                values.append(child.text or "")
            elif child.tag == "enclosure" and version_name in child.attrib:
                values.append(child.attrib[version_name])
        if len(values) != 1:
            raise ValueError("each direct RSS item must have exactly one Sparkle version")
        value = values[0]
        if re.fullmatch(r"[1-9][0-9]*", value) is None:
            raise ValueError(f"non-canonical sparkle version {value!r}")
        result.append(int(value))

    # Reject version-shaped lookalikes and supported representations outside
    # the direct rss/channel/item boundary rather than silently ignoring them.
    accepted_nodes = {id(item) for item in items}
    for parent in root.iter():
        for child in parent:
            local_name = child.tag.rsplit("}", 1)[-1] if isinstance(child.tag, str) else ""
            if local_name == "version" and not (id(parent) in accepted_nodes and child.tag == version_name):
                raise ValueError("misplaced or foreign-namespace Sparkle version")
            version_attributes = [
                name for name in child.attrib
                if (name.rsplit("}", 1)[-1] if isinstance(name, str) else "") == "version"
            ]
            if version_attributes and not (
                id(parent) in accepted_nodes
                and child.tag == "enclosure"
                and version_attributes == [version_name]
            ):
                raise ValueError("misplaced or foreign-namespace Sparkle enclosure version")
    if not result:
        raise ValueError("feed has no valid Sparkle versions")
    if len(set(result)) != len(result):
        raise ValueError("feed contains duplicate Sparkle versions")
    return result


def load_stable_state(path: Path = STABLE_CONFIG) -> tuple[int, str]:
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read stable configuration {path}: {error}") from error
    if not isinstance(config, dict) or set(config) != {
        "schema", "active_generation", "active_public_key"
    }:
        raise ValueError("stable configuration has unknown or missing fields")
    if config["schema"] != 1:
        raise ValueError("unsupported stable configuration schema")
    generation = config["active_generation"]
    if type(generation) is not int or generation < 1:
        raise ValueError("stable active generation must be positive")
    key = config["active_public_key"]
    if not isinstance(key, str):
        raise ValueError("stable active public key is missing")
    try:
        decoded = base64.b64decode(key, validate=True)
    except (ValueError, binascii.Error) as error:
        raise ValueError("invalid stable active public key") from error
    if len(decoded) != 32:
        raise ValueError("invalid stable active public key byte length")
    return generation, key


def load_created_stable_generations(
    path: Path | None,
    *,
    active: int,
    active_public_key: str,
) -> dict[int, tuple[str, int]]:
    """Read generation inventory from an already GitHub-attested site manifest."""
    if path is None:
        if active != 1:
            raise ValueError("rotated stable configuration requires authenticated prior site state")
        return {}
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read authenticated stable site manifest {path}: {error}") from error
    if not isinstance(manifest, dict) or set(manifest) != {
        "schema", "archive", "files", "feed_keys", "signed_by_generation"
    }:
        raise ValueError("authenticated stable site manifest has unknown or missing fields")
    if manifest["schema"] != 1:
        raise ValueError("unsupported authenticated stable site manifest schema")
    archive = manifest["archive"]
    if not isinstance(archive, dict) or set(archive) != {"name", "sha256", "bytes"}:
        raise ValueError("authenticated stable site archive record is malformed")
    if archive["name"] != "stable-pages-site.tar.gz":
        raise ValueError("authenticated stable site archive name is invalid")
    if not isinstance(archive["sha256"], str) or SHA256_RE.fullmatch(archive["sha256"]) is None:
        raise ValueError("authenticated stable site archive digest is invalid")
    if type(archive["bytes"]) is not int or archive["bytes"] < 1:
        raise ValueError("authenticated stable site archive size is invalid")
    records = manifest["feed_keys"]
    if not isinstance(records, list) or not records:
        raise ValueError("authenticated stable site generation inventory is missing")
    generations: list[int] = []
    for record in records:
        if not isinstance(record, dict) or set(record) != {"generation", "public_key"}:
            raise ValueError("authenticated stable site generation inventory is malformed")
        generation = record["generation"]
        if type(generation) is not int or generation < 1:
            raise ValueError("authenticated stable generation must be positive")
        key = record["public_key"]
        if not isinstance(key, str):
            raise ValueError("authenticated stable generation key is missing")
        try:
            decoded = base64.b64decode(key, validate=True)
        except (ValueError, binascii.Error) as error:
            raise ValueError("invalid authenticated stable generation key") from error
        if len(decoded) != 32:
            raise ValueError("invalid authenticated stable generation key byte length")
        generations.append(generation)
    if generations != list(range(1, max(generations) + 1)):
        raise ValueError("authenticated stable generations must be ordered and contiguous from v1")
    if max(generations) not in (active - 1, active):
        raise ValueError("authenticated stable generation state does not match reviewed config")
    if max(generations) == active and records[-1]["public_key"] != active_public_key:
        raise ValueError("authenticated active stable key does not match reviewed config")
    if manifest["signed_by_generation"] != max(generations):
        raise ValueError("authenticated stable manifest signer is not the newest generation")
    files = manifest["files"]
    if not isinstance(files, list) or not files:
        raise ValueError("authenticated stable site file inventory is missing")
    paths: list[str] = []
    file_records: dict[str, tuple[str, int]] = {}
    for record in files:
        if not isinstance(record, dict) or set(record) != {"path", "sha256", "bytes"}:
            raise ValueError("authenticated stable site file record is malformed")
        relative = record["path"]
        if not isinstance(relative, str):
            raise ValueError("authenticated stable site file path is invalid")
        parsed = PurePosixPath(relative)
        if (
            not relative
            or parsed.is_absolute()
            or parsed.as_posix() != relative
            or any(part in ("", ".", "..") for part in parsed.parts)
            or "\\" in relative
            or "\x00" in relative
        ):
            raise ValueError("authenticated stable site file path is unsafe")
        digest = record["sha256"]
        size = record["bytes"]
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise ValueError("authenticated stable site file digest is invalid")
        if type(size) is not int or size < 0:
            raise ValueError("authenticated stable site file size is invalid")
        paths.append(relative)
        if relative in file_records:
            raise ValueError("authenticated stable site file paths are duplicated")
        file_records[relative] = (digest, size)
    if paths != sorted(paths):
        raise ValueError("authenticated stable site file inventory is not ordered")

    feed_paths = {
        path for path in paths
        if re.fullmatch(r"updates/stable/v[1-9][0-9]*/stable\.xml", path)
    }
    feed_like_paths = {
        path for path in paths
        if path.startswith("updates/stable/") and path.endswith("/stable.xml")
    }
    expected_paths = {f"updates/stable/v{generation}/stable.xml" for generation in generations}
    if feed_paths != expected_paths or feed_like_paths != expected_paths:
        raise ValueError("authenticated stable feed inventory does not match generation keys")
    return {
        generation: file_records[f"updates/stable/v{generation}/stable.xml"]
        for generation in generations
    }


def feed_endpoints(
    mode: str,
    stable_config: Path = STABLE_CONFIG,
    stable_site_manifest: Path | None = None,
) -> list[FeedEndpoint]:
    """Return endpoint/required bindings from reviewed and authenticated state."""
    if mode == "test":
        return [FeedEndpoint(TEST_FEED)]
    if mode != "stable-candidate":
        raise ValueError(f"unsupported candidate mode {mode!r}")
    active, active_public_key = load_stable_state(stable_config)
    created = load_created_stable_generations(
        stable_site_manifest,
        active=active,
        active_public_key=active_public_key,
    )
    endpoints = [FeedEndpoint(TEST_FEED)]
    endpoints.extend(
        FeedEndpoint(
            STABLE_FEED_TEMPLATE.format(generation=generation),
            *(created[generation] if generation in created else (None, None)),
            absent_only=generation not in created,
        )
        for generation in range(1, active + 1)
    )
    return endpoints


def highest(
    mode: str,
    loader: Fetch = fetch,
    *,
    stable_config: Path = STABLE_CONFIG,
    stable_site_manifest: Path | None = None,
) -> tuple[int, list[str]]:
    observed: list[int] = []
    present: list[str] = []
    for endpoint in feed_endpoints(mode, stable_config, stable_site_manifest):
        payload = loader(endpoint.url)
        if payload is None:
            if endpoint.expected_sha256 is not None:
                raise ValueError(f"required prior stable feed is missing: {endpoint.url}")
            continue
        if endpoint.absent_only:
            raise ValueError(f"unrecorded stable feed returned unauthenticated bytes: {endpoint.url}")
        if endpoint.expected_bytes is not None and len(payload) != endpoint.expected_bytes:
            raise ValueError(f"authenticated stable feed byte count changed: {endpoint.url}")
        if endpoint.expected_sha256 is not None and sha256(payload).hexdigest() != endpoint.expected_sha256:
            raise ValueError(f"authenticated stable feed digest changed: {endpoint.url}")
        observed.extend(builds(payload))
        present.append(endpoint.url)
    return max(observed, default=0), present


def select_build(*, git_build: int, published_highest: int) -> int:
    """Select one deterministic build above both the git and public floors."""
    if git_build < 1 or published_highest < 0:
        raise ValueError("build floors must be positive git and non-negative published integers")
    return max(git_build, published_highest + 1)


def validate_selected_build(*, candidate_build: int, published_highest: int) -> None:
    """Close the race between preflight selection and immutable handoff."""
    if candidate_build < 1 or candidate_build <= published_highest:
        raise ValueError(
            f"selected build {candidate_build} is no longer newer than published build "
            f"{published_highest}"
        )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("test", "stable-candidate"), required=True)
    parser.add_argument(
        "--stable-site-manifest",
        type=Path,
        help="GitHub-attested prior stable site manifest used as creation-state evidence",
    )
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument(
        "--git-build",
        type=int,
        help="git rev-list count used as the deterministic local build floor",
    )
    selection.add_argument(
        "--candidate-build",
        type=int,
        help="already packaged build to revalidate immediately before handoff",
    )
    args = parser.parse_args(argv)
    try:
        value, feeds = highest(args.mode, stable_site_manifest=args.stable_site_manifest)
        selected = None
        if args.git_build is not None:
            selected = select_build(git_build=args.git_build, published_highest=value)
        elif args.candidate_build is not None:
            validate_selected_build(candidate_build=args.candidate_build, published_highest=value)
    except (OSError, ValueError, ET.ParseError) as error:
        print(f"CANDIDATE BUILD ORDER FAILED: {error}", file=sys.stderr)
        return 1
    print(f"highest applicable published build: {value}; feeds present: {len(feeds)}")
    if selected is not None:
        print(f"selected candidate build: {selected}")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"minimum_build={value}\n")
            if selected is not None:
                handle.write(f"build_number={selected}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
