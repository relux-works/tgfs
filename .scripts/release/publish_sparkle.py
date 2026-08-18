#!/usr/bin/env python3
"""Verify candidate handoffs and build recoverable Sparkle publication assets.

This utility deliberately has no network or credential handling. Workflows use
the pinned Sparkle ``sign_update`` binary for EdDSA operations and this script
for the deterministic, testable parts of publication: exact candidate intake,
endpoint-isolated feeds, complete-site carry-forward, and safe archives.
"""
from __future__ import annotations

import argparse
import copy
import gzip
import json
import os
import re
import shutil
import sys
import tarfile
import tempfile
import xml.etree.ElementTree as ET
from base64 import b64decode
from email.utils import parsedate_to_datetime
from hashlib import sha256, sha512
from pathlib import Path
from typing import Iterable, Sequence

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE_NS)
SCHEMA = 1
TEAM_ID = "262RZ595FP"
REPOSITORY = "relux-works/tgfs"
SIGNATURE_RE = re.compile(
    r'^sparkle:edSignature="([A-Za-z0-9+/]{86}==)" (?:sparkle:)?length="([1-9][0-9]*)"$'
)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
TAG_RE = re.compile(r"^v([0-9]+\.[0-9]+\.[0-9]+)$")
TEST_PUBLIC_ED_KEY_B64 = "T8IBLvve21ObUHz78CLXdF0eWN7QgJPHd1eKlcFhqmo="
STABLE_PUBLIC_ED_KEY_B64 = "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE="
ED_P = 2**255 - 19
ED_L = 2**252 + 27742317777372353535851937790883648493
ED_D = (-121665 * pow(121666, ED_P - 2, ED_P)) % ED_P
ED_I = pow(2, (ED_P - 1) // 4, ED_P)


class PublicationError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise PublicationError(message)


def read_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PublicationError(f"cannot read JSON {path}: {error}") from error
    require(isinstance(value, dict), f"expected a JSON object in {path}")
    return value


def sha256_file(path: Path) -> str:
    digest = sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _ed_xrecover(y: int) -> int:
    xx = ((y * y - 1) * pow(ED_D * y * y + 1, ED_P - 2, ED_P)) % ED_P
    x = pow(xx, (ED_P + 3) // 8, ED_P)
    if (x * x - xx) % ED_P:
        x = x * ED_I % ED_P
    require((x * x - xx) % ED_P == 0, "invalid Ed25519 point")
    return x


def _ed_decode(encoded: bytes) -> tuple[int, int, int, int]:
    require(len(encoded) == 32, "invalid Ed25519 point length")
    y = int.from_bytes(encoded, "little") & ((1 << 255) - 1)
    require(y < ED_P, "non-canonical Ed25519 point")
    x = _ed_xrecover(y)
    sign = encoded[31] >> 7
    if (x & 1) != sign:
        x = ED_P - x
    require((y * y - x * x - 1 - ED_D * x * x * y * y) % ED_P == 0, "invalid Ed25519 point")
    return x, y, 1, x * y % ED_P


def _ed_add(p: tuple[int, int, int, int], q: tuple[int, int, int, int]) -> tuple[int, int, int, int]:
    x1, y1, z1, t1 = p
    x2, y2, z2, t2 = q
    a = (y1 - x1) * (y2 - x2) % ED_P
    b = (y1 + x1) * (y2 + x2) % ED_P
    c = 2 * ED_D * t1 * t2 % ED_P
    d = 2 * z1 * z2 % ED_P
    e, f, g, h = b - a, d - c, d + c, b + a
    return e * f % ED_P, g * h % ED_P, f * g % ED_P, e * h % ED_P


def _ed_scale(point: tuple[int, int, int, int], scalar: int) -> tuple[int, int, int, int]:
    result = (0, 1, 1, 0)
    while scalar:
        if scalar & 1:
            result = _ed_add(result, point)
        point = _ed_add(point, point)
        scalar >>= 1
    return result


def _ed_equal(p: tuple[int, int, int, int], q: tuple[int, int, int, int]) -> bool:
    return (p[0] * q[2] - q[0] * p[2]) % ED_P == 0 and (p[1] * q[2] - q[1] * p[2]) % ED_P == 0


ED_BASE = (_ed_xrecover(4 * pow(5, ED_P - 2, ED_P) % ED_P), 4 * pow(5, ED_P - 2, ED_P) % ED_P, 1, 0)
if ED_BASE[0] & 1:
    ED_BASE = (ED_P - ED_BASE[0], ED_BASE[1], 1, 0)
ED_BASE = (ED_BASE[0], ED_BASE[1], 1, ED_BASE[0] * ED_BASE[1] % ED_P)


def verify_ed25519(public_key: bytes, message: bytes, signature_bytes: bytes) -> bool:
    """Strict RFC 8032 Ed25519 verification for the checked-in public key."""
    if len(public_key) != 32 or len(signature_bytes) != 64:
        return False
    try:
        public_point = _ed_decode(public_key)
        r_point = _ed_decode(signature_bytes[:32])
    except PublicationError:
        return False
    scalar = int.from_bytes(signature_bytes[32:], "little")
    if scalar >= ED_L:
        return False
    identity = (0, 1, 1, 0)
    if _ed_equal(_ed_scale(public_point, 8), identity) or _ed_equal(_ed_scale(r_point, 8), identity):
        return False
    challenge = int.from_bytes(sha512(signature_bytes[:32] + public_key + message).digest(), "little") % ED_L
    return _ed_equal(_ed_scale(ED_BASE, scalar), _ed_add(r_point, _ed_scale(public_point, challenge)))


def _decode_b64(value: str, label: str, expected: int) -> bytes:
    try:
        decoded = b64decode(value, validate=True)
    except ValueError as error:
        raise PublicationError(f"invalid base64 {label}") from error
    require(len(decoded) == expected, f"invalid {label} byte length")
    return decoded


def public_key(value: str, label: str = "Sparkle public key") -> bytes:
    return _decode_b64(value, label, 32)


def parse_checksums(path: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as error:
        raise PublicationError(f"cannot read {path}: {error}") from error
    for number, line in enumerate(lines, 1):
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\]+)", line)
        require(match is not None, f"malformed or unsafe checksum line {path}:{number}")
        digest, name = match.groups()
        require(name not in result, f"duplicate checksum entry {name!r}")
        result[name] = digest
    require(result, f"empty checksum inventory: {path}")
    return result


def verify_candidate(
    root: Path,
    *,
    repository: str = REPOSITORY,
    run_id: str | None = None,
    commit: str | None = None,
    mode: str | None = None,
    version: str | None = None,
) -> dict:
    """Revalidate exact package bytes and the publication-facing semantics."""
    root = root.resolve()
    require(root.is_dir(), f"candidate directory is missing: {root}")
    root_entries = list(root.iterdir())
    require(
        root_entries and all(path.is_file() and not path.is_symlink() for path in root_entries),
        "candidate package must be a flat regular-file inventory",
    )
    checksums = parse_checksums(root / "CANDIDATE-CHECKSUMS.sha256")
    actual = {
        path.name for path in root.iterdir()
        if path.is_file() and not path.is_symlink() and path.name != "CANDIDATE-CHECKSUMS.sha256"
    }
    require(set(checksums) == actual, "candidate checksum inventory does not exactly match package")
    for name, digest in checksums.items():
        require(sha256_file(root / name) == digest, f"candidate checksum mismatch: {name}")

    manifest = read_json(root / "candidate-manifest.json")
    verification = read_json(root / "verification.json")
    finalization = read_json(root / "finalization.json")
    app = read_json(root / "app-manifest.json")
    provenance = read_json(root / "candidate-provenance.json")
    require(manifest.get("schema") == SCHEMA, "unsupported candidate manifest schema")
    require(verification.get("schema") == SCHEMA and verification.get("result") == "passed", "candidate verification did not pass")
    gates = verification.get("gates")
    require(isinstance(gates, dict) and gates and set(gates.values()) == {"passed"}, "candidate verification gates are incomplete")
    require(finalization.get("schema") == SCHEMA and finalization.get("status") == "verified-and-attested", "candidate is not finalized")
    require(finalization.get("privacy_scrub") == "passed", "candidate privacy scrub did not pass")
    attestation = root / "candidate-attestation.json"
    require(
        sha256_file(attestation) == finalization.get("attestation", {}).get("sha256"),
        "candidate attestation bytes do not match finalization",
    )
    source = manifest.get("source", {})
    workflow = manifest.get("workflow", {})
    require(source.get("repository") == repository, "candidate repository is not the approved source")
    source_commit = str(source.get("commit", ""))
    require(COMMIT_RE.fullmatch(source_commit) is not None, "candidate source commit is invalid")
    require(source == provenance.get("source"), "candidate source and provenance disagree")
    require(workflow == provenance.get("workflow"), "candidate workflow and provenance disagree")
    require(
        str(workflow.get("ref", "")).startswith(f"{repository}/.github/workflows/candidate-build.yml@"),
        "candidate was not produced by the approved candidate workflow",
    )
    require(str(workflow.get("run_id", "")).isdigit(), "candidate run id is invalid")
    require(str(workflow.get("run_attempt", "")).isdigit(), "candidate run attempt is invalid")
    if run_id is not None:
        require(str(workflow.get("run_id")) == str(run_id), "candidate came from a different workflow run")
    if commit is not None:
        require(source_commit == commit, "candidate commit does not match the requested source")
    if mode is not None:
        require(manifest.get("mode") == mode, "candidate mode does not match publication request")
    if version is not None:
        require(manifest.get("version", {}).get("short") == version, "candidate version does not match release tag")
    expected_channel = "stable" if manifest.get("mode") == "stable-candidate" else "test"
    require(manifest.get("mode") in ("test", "stable-candidate"), "candidate mode is invalid")
    require(manifest.get("channel") == expected_channel, "candidate channel does not match its mode")
    require(provenance.get("candidate") == {"mode": manifest["mode"], "channel": expected_channel}, "candidate provenance channel disagrees")
    require(manifest.get("publication") == {"owned": False, "downstream_task": "TASK-260810-y3zcg8"}, "candidate does not delegate publication to this task")
    identity = manifest.get("identity", {})
    require(identity.get("team_id") == TEAM_ID, "candidate Team ID is invalid")
    dmg = manifest.get("dmg", {})
    dmg_name = str(dmg.get("name", ""))
    require(re.fullmatch(r"GramDrive-[0-9]+\.[0-9]+\.[0-9]+-[1-9][0-9]*\.dmg", dmg_name) is not None, "candidate DMG name is not immutable/build-named")
    dmg_path = root / dmg_name
    require(dmg_path.is_file() and not dmg_path.is_symlink(), "candidate DMG is missing")
    require(SHA256_RE.fullmatch(str(dmg.get("sha256", ""))) is not None, "candidate DMG digest is invalid")
    require(sha256_file(dmg_path) == dmg["sha256"], "candidate DMG exact bytes changed")
    require(dmg_path.stat().st_size == dmg.get("bytes"), "candidate DMG length changed")
    product = manifest.get("version", {})
    require(VERSION_RE.fullmatch(str(product.get("short", ""))) is not None, "candidate marketing version is invalid")
    require(str(product.get("build", "")).isdigit(), "candidate Sparkle build is invalid")
    sparkle = app.get("sparkle", {})
    require(sparkle.get("channel") == expected_channel, "embedded Sparkle channel does not match candidate")
    embedded = app.get("sparkle", {})
    generation = embedded.get("generation", 1)
    require(isinstance(generation, int) and generation > 0, "embedded Sparkle feed generation is invalid")
    embedded_key = str(embedded.get("public_key", ""))
    if not embedded_key and generation == 1:
        embedded_key = TEST_PUBLIC_ED_KEY_B64 if expected_channel == "test" else STABLE_PUBLIC_ED_KEY_B64
    public_key(embedded_key, "embedded Sparkle public key")
    expected_feed = feed_urls(expected_channel, generation, repository, str(product["short"]), dmg_name, "notes.md")[0]
    require(sparkle.get("feed_url") == expected_feed, "embedded Sparkle feed URL does not match candidate channel")
    sparkle["generation"] = generation
    sparkle["public_key"] = embedded_key
    manifest["sparkle"] = sparkle
    return manifest


def github_outputs(manifest: dict) -> dict[str, str]:
    return {
        "build": str(manifest["version"]["build"]),
        "version": str(manifest["version"]["short"]),
        "commit": str(manifest["source"]["commit"]),
        "mode": str(manifest["mode"]),
        "channel": str(manifest["channel"]),
        "dmg_name": str(manifest["dmg"]["name"]),
        "dmg_sha256": str(manifest["dmg"]["sha256"]),
        "run_id": str(manifest["workflow"]["run_id"]),
        "run_attempt": str(manifest["workflow"]["run_attempt"]),
        "feed_generation": str(manifest["sparkle"]["generation"]),
        "feed_public_key": str(manifest["sparkle"]["public_key"]),
    }


def emit_outputs(values: dict[str, str]) -> None:
    for key, value in values.items():
        print(f"{key}={value}")
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as handle:
            for key, value in values.items():
                handle.write(f"{key}={value}\n")


def _tar_info(path: Path, name: str) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.size = path.stat().st_size
    info.mode = 0o644
    info.mtime = 0
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


def pack_tree(root: Path, output: Path) -> None:
    root = root.resolve()
    files = sorted(path for path in root.rglob("*") if path.is_file() and not path.is_symlink())
    require(files, f"refusing to archive empty tree: {root}")
    require(all(not path.is_symlink() for path in root.rglob("*")), "archive source contains a symlink")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(mode="w", fileobj=compressed) as archive:
                for path in files:
                    name = path.relative_to(root).as_posix()
                    with path.open("rb") as handle:
                        archive.addfile(_tar_info(path, name), handle)


def unpack_tree(archive_path: Path, output: Path, *, flat: bool = False) -> None:
    output = output.resolve()
    require(not output.exists(), f"archive destination already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            members = archive.getmembers()
            require(members, "archive is empty")
            require(len(members) <= 128, "archive contains too many members")
            require(sum(member.size for member in members) <= 10 * 1024**3, "archive expands beyond 10 GiB")
            names: set[str] = set()
            for member in members:
                path = Path(member.name)
                require(member.isfile(), f"archive member is not a regular file: {member.name}")
                require(not path.is_absolute() and ".." not in path.parts, f"unsafe archive member: {member.name}")
                require(not flat or len(path.parts) == 1, f"candidate archive member is not flat: {member.name}")
                require(member.name not in names, f"duplicate archive member: {member.name}")
                names.add(member.name)
                target = scratch / path
                target.parent.mkdir(parents=True, exist_ok=True)
                source = archive.extractfile(member)
                require(source is not None, f"cannot read archive member: {member.name}")
                with target.open("wb") as handle:
                    shutil.copyfileobj(source, handle)
        scratch.rename(output)
    except Exception:
        shutil.rmtree(scratch, ignore_errors=True)
        raise


def signature(path: Path) -> tuple[str, int]:
    try:
        text = path.read_text(encoding="utf-8").strip()
    except (OSError, UnicodeDecodeError) as error:
        raise PublicationError(f"cannot read Sparkle signature output {path}: {error}") from error
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    matches = [SIGNATURE_RE.fullmatch(line) for line in lines]
    matches = [match for match in matches if match is not None]
    require(len(matches) == 1, f"malformed Sparkle signature output in {path}")
    require(
        all(SIGNATURE_RE.fullmatch(line) or (line.startswith("<!-- Updated ") and line.endswith(" -->")) for line in lines),
        f"unexpected Sparkle signing output in {path}",
    )
    match = matches[0]
    return match.group(1), int(match.group(2))


def feed_urls(channel: str, generation: int, repository: str, version: str, dmg_name: str, notes_name: str) -> tuple[str, str, str]:
    if channel == "test":
        base = f"https://github.com/{repository}/releases/download/updates-test-v{generation}"
        return f"{base}/test.xml", f"{base}/{dmg_name}", f"{base}/{notes_name}"
    tag = f"v{version}"
    feed = f"https://relux-works.github.io/tgfs/updates/stable/v{generation}/stable.xml"
    enclosure = f"https://github.com/{repository}/releases/download/{tag}/{dmg_name}"
    notes = f"https://relux-works.github.io/tgfs/updates/stable/v{generation}/notes/{notes_name}"
    return feed, enclosure, notes


def _item_build(item: ET.Element) -> int:
    value = item.findtext(f"{{{SPARKLE_NS}}}version")
    if value is None:
        enclosure = item.find("enclosure")
        value = enclosure.get(f"{{{SPARKLE_NS}}}version") if enclosure is not None else None
    require(value is not None and value.isdigit(), "feed item has a non-numeric Sparkle version")
    return int(value)


def _load_items(path: Path | None) -> list[ET.Element]:
    if path is None or not path.exists():
        return []
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise PublicationError(f"cannot parse prior feed {path}: {error}") from error
    require(root.tag == "rss", "prior feed root is not rss")
    channel = root.find("channel")
    require(channel is not None, "prior feed has no channel")
    return [copy.deepcopy(item) for item in channel.findall("item")]


def validate_items(items: Iterable[ET.Element], *, channel: str, generation: int, repository: str) -> list[int]:
    builds: list[int] = []
    seen: set[int] = set()
    test_prefix = f"https://github.com/{repository}/releases/download/updates-test-v{generation}/"
    stable_prefix = f"https://github.com/{repository}/releases/download/v"
    notes_prefix = f"https://relux-works.github.io/tgfs/updates/stable/v{generation}/notes/"
    for item in items:
        build = _item_build(item)
        require(build not in seen, f"duplicate Sparkle build {build}")
        seen.add(build)
        builds.append(build)
        enclosure = item.find("enclosure")
        require(enclosure is not None, f"build {build} has no enclosure")
        url = str(enclosure.get("url", ""))
        require(url.startswith(test_prefix if channel == "test" else stable_prefix), f"build {build} escapes the {channel} endpoint")
        short_version = item.findtext(f"{{{SPARKLE_NS}}}shortVersionString")
        require(short_version is not None and VERSION_RE.fullmatch(short_version) is not None, f"build {build} has an invalid short version")
        if channel == "test":
            require(
                re.fullmatch(re.escape(test_prefix) + rf"GramDrive-{re.escape(short_version)}-{build}\.dmg", url) is not None,
                f"build {build} test enclosure is not immutable/build-named",
            )
        else:
            require(
                url == f"https://github.com/{repository}/releases/download/v{short_version}/GramDrive-{short_version}-{build}.dmg",
                f"build {build} stable enclosure tag/version/name disagree",
            )
        require(enclosure.get(f"{{{SPARKLE_NS}}}edSignature") is not None, f"build {build} has no EdDSA enclosure signature")
        require(str(enclosure.get("length", "")).isdigit(), f"build {build} has invalid enclosure length")
        notes = item.find(f"{{{SPARKLE_NS}}}releaseNotesLink")
        require(notes is not None and notes.text, f"build {build} has no release notes")
        notes_url = notes.text.strip()
        require(
            notes_url.startswith(test_prefix) if channel == "test" else notes_url.startswith(notes_prefix),
            f"build {build} release notes escape the {channel} endpoint",
        )
        expected_notes_prefix = test_prefix if channel == "test" else notes_prefix
        require(
            notes_url == f"{expected_notes_prefix}GramDrive-{short_version}-{build}.md",
            f"build {build} release notes are not version/build named",
        )
        require(notes.get(f"{{{SPARKLE_NS}}}edSignature") is not None, f"build {build} release notes are unsigned")
        require(str(notes.get(f"{{{SPARKLE_NS}}}length", "")).isdigit(), f"build {build} release notes length is invalid")
    require(builds == sorted(builds, reverse=True), "feed items are not in descending build order")
    return builds


def render_feed(
    manifest: dict,
    *,
    channel: str,
    generation: int,
    repository: str,
    dmg_signature_path: Path,
    notes_signature_path: Path,
    notes_name: str,
    publication_date: str,
    prior_feed: Path | None,
    output: Path,
) -> None:
    require(channel in ("test", "stable"), "invalid publication channel")
    require(generation > 0, "feed generation must be positive")
    accepted_modes = ("test", "stable-candidate") if channel == "test" else ("stable-candidate",)
    require(manifest.get("mode") in accepted_modes, f"{channel} feed refuses {manifest.get('mode')} candidate")
    try:
        published = parsedate_to_datetime(publication_date)
    except (TypeError, ValueError) as error:
        raise PublicationError(f"invalid RFC 2822 publication date: {publication_date}") from error
    require(published.tzinfo is not None, "publication date must include a timezone")
    dmg_sig, dmg_length = signature(dmg_signature_path)
    notes_sig, notes_length = signature(notes_signature_path)
    require(dmg_length == manifest["dmg"]["bytes"], "Sparkle signed length does not match candidate DMG")
    version = str(manifest["version"]["short"])
    build = int(manifest["version"]["build"])
    feed_url, enclosure_url, notes_url = feed_urls(channel, generation, repository, version, manifest["dmg"]["name"], notes_name)

    old_items = _load_items(prior_feed)
    if old_items:
        validate_items(old_items, channel=channel, generation=generation, repository=repository)
    old_items = [item for item in old_items if _item_build(item) != build]
    require(not old_items or build > max(_item_build(item) for item in old_items), "candidate build is not newer than the feed")

    item = ET.Element("item")
    ET.SubElement(item, "title").text = f"GramDrive {version} ({build})"
    ET.SubElement(item, f"{{{SPARKLE_NS}}}version").text = str(build)
    ET.SubElement(item, f"{{{SPARKLE_NS}}}shortVersionString").text = version
    ET.SubElement(item, "pubDate").text = publication_date
    ET.SubElement(item, f"{{{SPARKLE_NS}}}minimumSystemVersion").text = "14.0.0"
    notes = ET.SubElement(item, f"{{{SPARKLE_NS}}}releaseNotesLink")
    notes.text = notes_url
    notes.set(f"{{{SPARKLE_NS}}}edSignature", notes_sig)
    notes.set(f"{{{SPARKLE_NS}}}length", str(notes_length))
    enclosure = ET.SubElement(item, "enclosure")
    enclosure.set("url", enclosure_url)
    enclosure.set("length", str(dmg_length))
    enclosure.set("type", "application/octet-stream")
    enclosure.set(f"{{{SPARKLE_NS}}}edSignature", dmg_sig)

    items = [item] + old_items
    items.sort(key=_item_build, reverse=True)
    if channel == "test":
        items = items[:11]
    validate_items(items, channel=channel, generation=generation, repository=repository)
    rss = ET.Element("rss", {"version": "2.0"})
    channel_node = ET.SubElement(rss, "channel")
    ET.SubElement(channel_node, "title").text = f"GramDrive {channel} updates v{generation}"
    ET.SubElement(channel_node, "link").text = feed_url
    ET.SubElement(channel_node, "description").text = f"Sparkle-signed GramDrive {channel} updates"
    ET.SubElement(channel_node, "language").text = "en"
    for update in items:
        channel_node.append(update)
    ET.indent(rss, space="  ")
    output.parent.mkdir(parents=True, exist_ok=True)
    ET.ElementTree(rss).write(output, encoding="utf-8", xml_declaration=True)


def validate_feed(path: Path, *, channel: str, generation: int, repository: str) -> None:
    items = _load_items(path)
    require(items, "feed contains no items")
    validate_items(items, channel=channel, generation=generation, repository=repository)


def signed_content(payload: bytes, *, label: str = "file") -> tuple[bytes, bytes]:
    marker = b"<!-- sparkle-signatures:\nedSignature: "
    position = payload.rfind(marker)
    require(position >= 0, f"{label} has no embedded Sparkle signature block")
    content = payload[:position]
    block = payload[position:]
    match = re.fullmatch(
        rb"<!-- sparkle-signatures:\nedSignature: ([A-Za-z0-9+/]{86}==)\nlength: ([1-9][0-9]*)\n-->\n",
        block,
    )
    require(match is not None, f"{label} signature block is malformed or not terminal")
    require(int(match.group(2)) == len(content), f"{label} signed length is wrong")
    return content, _decode_b64(match.group(1).decode("ascii"), "feed signature", 64)


def signed_feed_content(payload: bytes) -> tuple[bytes, bytes]:
    return signed_content(payload, label="test feed")


def verify_test_offer(
    candidate_dir: Path,
    feed_path: Path,
    notes_path: Path,
    *,
    repository: str = REPOSITORY,
    generation: int = 1,
    test_public_key_b64: str = TEST_PUBLIC_ED_KEY_B64,
    verifier=verify_ed25519,
) -> None:
    manifest = verify_candidate(candidate_dir, repository=repository, mode="stable-candidate")
    test_key = public_key(test_public_key_b64, "test public key")
    payload = feed_path.read_bytes()
    content, feed_signature = signed_feed_content(payload)
    require(verifier(test_key, content, feed_signature), "test feed EdDSA signature is invalid")
    try:
        root = ET.fromstring(content)
    except ET.ParseError as error:
        raise PublicationError(f"signed test feed XML is invalid: {error}") from error
    channel = root.find("channel")
    require(channel is not None, "signed test feed has no channel")
    validate_items(channel.findall("item"), channel="test", generation=generation, repository=repository)
    build = int(manifest["version"]["build"])
    matches = [item for item in channel.findall("item") if _item_build(item) == build]
    require(len(matches) == 1, "stable candidate is not uniquely offered by the signed test feed")
    item = matches[0]
    enclosure = item.find("enclosure")
    require(enclosure is not None, "stable candidate test offer has no enclosure")
    expected_base = f"https://github.com/{repository}/releases/download/updates-test-v{generation}/"
    require(enclosure.get("url") == expected_base + manifest["dmg"]["name"], "test offer enclosure URL is not the verified candidate")
    require(int(str(enclosure.get("length", "0"))) == manifest["dmg"]["bytes"], "test offer enclosure length changed")
    enclosure_signature = _decode_b64(str(enclosure.get(f"{{{SPARKLE_NS}}}edSignature", "")), "enclosure signature", 64)
    require(verifier(test_key, (candidate_dir / manifest["dmg"]["name"]).read_bytes(), enclosure_signature), "test offer enclosure EdDSA signature is invalid")
    notes = item.find(f"{{{SPARKLE_NS}}}releaseNotesLink")
    require(notes is not None and notes.text, "stable candidate test offer has no release notes")
    require(notes.text.strip() == expected_base + notes_path.name, "test offer release-notes URL is not the downloaded asset")
    require(int(str(notes.get(f"{{{SPARKLE_NS}}}length", "0"))) == notes_path.stat().st_size, "test offer release-notes length changed")
    notes_signature = _decode_b64(str(notes.get(f"{{{SPARKLE_NS}}}edSignature", "")), "release-notes signature", 64)
    require(verifier(test_key, notes_path.read_bytes(), notes_signature), "test offer release-notes EdDSA signature is invalid")


def parse_generation_keys(values: Sequence[str]) -> dict[int, str]:
    keys: dict[int, str] = {}
    for value in values:
        match = re.fullmatch(r"([1-9][0-9]*)=([A-Za-z0-9+/]{43}=)", value)
        require(match is not None, f"invalid generation/public-key binding: {value!r}")
        generation = int(match.group(1))
        key = match.group(2)
        public_key(key, f"v{generation} public key")
        require(generation not in keys or keys[generation] == key, f"conflicting public keys for feed generation v{generation}")
        keys[generation] = key
    require(keys, "at least one generation/public-key binding is required")
    return keys


def site_inventory(site: Path) -> list[dict[str, object]]:
    site = site.resolve()
    require(site.is_dir(), f"stable site directory is missing: {site}")
    require(all(not path.is_symlink() for path in site.rglob("*")), "stable site contains a symlink")
    files = sorted(path for path in site.rglob("*") if path.is_file())
    require(files, "stable site is empty")
    return [
        {"path": path.relative_to(site).as_posix(), "sha256": sha256_file(path), "bytes": path.stat().st_size}
        for path in files
    ]


def write_site_manifest(
    site: Path,
    archive: Path,
    generation_keys: dict[int, str],
    signer_generation: int,
    output: Path,
) -> None:
    require(signer_generation in generation_keys, "site manifest signer generation has no public key")
    value = {
        "schema": SCHEMA,
        "archive": {"name": archive.name, "sha256": sha256_file(archive), "bytes": archive.stat().st_size},
        "files": site_inventory(site),
        "feed_keys": [
            {"generation": generation, "public_key": generation_keys[generation]}
            for generation in sorted(generation_keys)
        ],
        "signed_by_generation": signer_generation,
    }
    output.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def verify_site(
    site: Path,
    archive: Path,
    manifest_path: Path,
    manifest_signature_path: Path,
    *,
    repository: str = REPOSITORY,
    verifier=verify_ed25519,
) -> None:
    manifest = read_json(manifest_path)
    require(manifest.get("schema") == SCHEMA, "unsupported stable site manifest schema")
    archive_record = manifest.get("archive", {})
    require(archive_record.get("name") == archive.name, "stable site archive name changed")
    require(archive_record.get("sha256") == sha256_file(archive), "stable site archive digest changed")
    require(archive_record.get("bytes") == archive.stat().st_size, "stable site archive length changed")
    require(manifest.get("files") == site_inventory(site), "stable site extracted inventory changed")
    key_records = manifest.get("feed_keys")
    require(isinstance(key_records, list), "stable site feed key inventory is missing")
    keys = parse_generation_keys(
        [f"{record.get('generation')}={record.get('public_key')}" for record in key_records if isinstance(record, dict)]
    )
    signer_generation = manifest.get("signed_by_generation")
    require(isinstance(signer_generation, int) and signer_generation in keys, "stable site manifest signer is invalid")
    manifest_sig, manifest_length = signature(manifest_signature_path)
    require(manifest_length == manifest_path.stat().st_size, "stable site manifest signed length changed")
    require(
        verifier(public_key(keys[signer_generation]), manifest_path.read_bytes(), _decode_b64(manifest_sig, "site manifest signature", 64)),
        "stable site manifest EdDSA signature is invalid",
    )
    expected_feeds = {f"updates/stable/v{generation}/stable.xml" for generation in keys}
    actual_feeds = {path.relative_to(site).as_posix() for path in site.glob("updates/stable/v*/stable.xml")}
    require(actual_feeds == expected_feeds, "stable site feed generations do not exactly match key inventory")
    for generation, key_b64 in keys.items():
        feed_path = site / f"updates/stable/v{generation}/stable.xml"
        content, feed_signature = signed_content(feed_path.read_bytes(), label=f"stable v{generation} feed")
        key = public_key(key_b64, f"stable v{generation} public key")
        require(verifier(key, content, feed_signature), f"stable v{generation} feed EdDSA signature is invalid")
        try:
            root = ET.fromstring(content)
        except ET.ParseError as error:
            raise PublicationError(f"stable v{generation} feed XML is invalid: {error}") from error
        channel = root.find("channel")
        require(channel is not None, f"stable v{generation} feed has no channel")
        items = channel.findall("item")
        validate_items(items, channel="stable", generation=generation, repository=repository)
        for item in items:
            notes = item.find(f"{{{SPARKLE_NS}}}releaseNotesLink")
            require(notes is not None and notes.text, "stable feed item has no release notes")
            prefix = f"https://relux-works.github.io/tgfs/updates/stable/v{generation}/"
            require(notes.text.startswith(prefix), "stable release-notes URL generation changed")
            relative = notes.text[len("https://relux-works.github.io/tgfs/"):]
            notes_path = site / relative
            require(notes_path.is_file(), f"stable release notes are missing: {relative}")
            notes_sig = _decode_b64(str(notes.get(f"{{{SPARKLE_NS}}}edSignature", "")), "release-notes signature", 64)
            require(int(str(notes.get(f"{{{SPARKLE_NS}}}length", "0"))) == notes_path.stat().st_size, "stable release-notes length changed")
            require(verifier(key, notes_path.read_bytes(), notes_sig), f"stable v{generation} release-notes signature is invalid")


def write_notes(manifest: dict, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        "\n".join(
            (
                f"# GramDrive {manifest['version']['short']} ({manifest['version']['build']})",
                "",
                "This update was promoted from the verified immutable candidate package.",
                "",
                f"- Source commit: `{manifest['source']['commit']}`",
                f"- Candidate run: `{manifest['workflow']['run_id']}` attempt `{manifest['workflow']['run_attempt']}`",
                f"- DMG SHA-256: `{manifest['dmg']['sha256']}`",
                "",
            )
        ),
        encoding="utf-8",
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    inspect = commands.add_parser("inspect-candidate")
    inspect.add_argument("--candidate-dir", type=Path, required=True)
    inspect.add_argument("--repository", default=REPOSITORY)
    inspect.add_argument("--run-id")
    inspect.add_argument("--commit")
    inspect.add_argument("--mode", choices=("test", "stable-candidate"))
    inspect.add_argument("--version")
    pack = commands.add_parser("pack-tree")
    pack.add_argument("--root", type=Path, required=True)
    pack.add_argument("--output", type=Path, required=True)
    unpack = commands.add_parser("unpack-tree")
    unpack.add_argument("--archive", type=Path, required=True)
    unpack.add_argument("--output", type=Path, required=True)
    unpack.add_argument("--flat", action="store_true")
    notes = commands.add_parser("write-notes")
    notes.add_argument("--candidate-dir", type=Path, required=True)
    notes.add_argument("--output", type=Path, required=True)
    render = commands.add_parser("render-feed")
    render.add_argument("--candidate-dir", type=Path, required=True)
    render.add_argument("--channel", choices=("test", "stable"), required=True)
    render.add_argument("--generation", type=int, required=True)
    render.add_argument("--repository", default=REPOSITORY)
    render.add_argument("--dmg-signature", type=Path, required=True)
    render.add_argument("--notes-signature", type=Path, required=True)
    render.add_argument("--notes-name", required=True)
    render.add_argument("--publication-date", required=True)
    render.add_argument("--prior-feed", type=Path)
    render.add_argument("--output", type=Path, required=True)
    validate = commands.add_parser("validate-feed")
    validate.add_argument("--feed", type=Path, required=True)
    validate.add_argument("--channel", choices=("test", "stable"), required=True)
    validate.add_argument("--generation", type=int, required=True)
    validate.add_argument("--repository", default=REPOSITORY)
    offer = commands.add_parser("verify-test-offer")
    offer.add_argument("--candidate-dir", type=Path, required=True)
    offer.add_argument("--feed", type=Path, required=True)
    offer.add_argument("--notes", type=Path, required=True)
    offer.add_argument("--repository", default=REPOSITORY)
    offer.add_argument("--generation", type=int, default=1)
    offer.add_argument("--test-public-key", default=TEST_PUBLIC_ED_KEY_B64)
    site_manifest = commands.add_parser("write-site-manifest")
    site_manifest.add_argument("--site", type=Path, required=True)
    site_manifest.add_argument("--archive", type=Path, required=True)
    site_manifest.add_argument("--key", action="append", required=True)
    site_manifest.add_argument("--signer-generation", type=int, required=True)
    site_manifest.add_argument("--output", type=Path, required=True)
    verify_site_parser = commands.add_parser("verify-site")
    verify_site_parser.add_argument("--site", type=Path, required=True)
    verify_site_parser.add_argument("--archive", type=Path, required=True)
    verify_site_parser.add_argument("--manifest", type=Path, required=True)
    verify_site_parser.add_argument("--manifest-signature", type=Path, required=True)
    verify_site_parser.add_argument("--repository", default=REPOSITORY)
    args = parser.parse_args(argv)
    try:
        if args.command == "inspect-candidate":
            manifest = verify_candidate(args.candidate_dir, repository=args.repository, run_id=args.run_id, commit=args.commit, mode=args.mode, version=args.version)
            emit_outputs(github_outputs(manifest))
        elif args.command == "pack-tree":
            pack_tree(args.root, args.output)
        elif args.command == "unpack-tree":
            unpack_tree(args.archive, args.output, flat=args.flat)
        elif args.command == "write-notes":
            write_notes(verify_candidate(args.candidate_dir), args.output)
        elif args.command == "render-feed":
            manifest = verify_candidate(args.candidate_dir, repository=args.repository)
            render_feed(manifest, channel=args.channel, generation=args.generation, repository=args.repository, dmg_signature_path=args.dmg_signature, notes_signature_path=args.notes_signature, notes_name=args.notes_name, publication_date=args.publication_date, prior_feed=args.prior_feed, output=args.output)
        elif args.command == "validate-feed":
            validate_feed(args.feed, channel=args.channel, generation=args.generation, repository=args.repository)
        elif args.command == "verify-test-offer":
            verify_test_offer(args.candidate_dir, args.feed, args.notes, repository=args.repository, generation=args.generation, test_public_key_b64=args.test_public_key)
        elif args.command == "write-site-manifest":
            write_site_manifest(args.site, args.archive, parse_generation_keys(args.key), args.signer_generation, args.output)
        else:
            verify_site(args.site, args.archive, args.manifest, args.manifest_signature, repository=args.repository)
    except (OSError, PublicationError, tarfile.TarError) as error:
        print(f"SPARKLE PUBLICATION FAILED: {error}", file=sys.stderr)
        return 1
    print(f"SPARKLE PUBLICATION {args.command.upper()} PASSED")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
