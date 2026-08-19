#!/usr/bin/env python3
"""Build and verify the immutable candidate handoff package.

This script owns the boundary between Apple packaging and downstream Sparkle
publication.  It never reads a credential.  It accepts only privacy-safe build
metadata, rejects an unverified/non-live artifact, copies the exact DMG bytes,
and can bind the resulting subjects to the Sigstore bundle emitted by
``actions/attest-build-provenance``.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shutil
import sys
from hashlib import sha256
from pathlib import Path
from typing import Iterable, Sequence

SCHEMA = 1
TEAM_ID = "262RZ595FP"
IDENTITY = f"Developer ID Application: Relux Works, LLC ({TEAM_ID})"
EXPECTED_BUNDLE_IDS = {
    "com.reluxworks.gramdrive",
    "com.reluxworks.gramdrive.agent",
    "com.reluxworks.gramdrive.fileprovider",
}
MODE_TO_CHANNEL = {"test": "test", "stable-candidate": "stable"}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
TEXT_SUFFIXES = {".json", ".md", ".txt", ".sha256"}
PINNED_TDLIB_REPO = "https://github.com/tdlib/td.git"
PINNED_TDLIB_COMMIT = "022d60202e446ad1287b9fb68e687c8a0760788b"
OPENSSL_LICENSE_PATH = "ThirdPartyLicenses/OpenSSL.txt"
APP_OPENSSL_LICENSE_PATH = f"GramDrive.app/Contents/Resources/{OPENSSL_LICENSE_PATH}"
PINNED_OPENSSL_VERSION = "3.6.3"
PINNED_OPENSSL_SOURCE_SHA256 = "243a86649cf6f23eeb6a2ff2456e09e5d77dd9018a54d3d96b0c6bdd6ba6c7f1"
OPENSSL_CERT_FILE = "/etc/ssl/cert.pem"
FORBIDDEN_RUNTIME_PATH_MARKERS = (
    b"/opt/homebrew/",
    b"/usr/local/opt/",
    b"/usr/local/Cellar/",
    b"/Users/",
    b"/home/",
    b"/private/tmp/",
    b"/var/folders/",
)
FORBIDDEN_TEXT = (
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    "-----BEGIN CERTIFICATE-----",
    "MACOS_CERT_P12",
    "MACOS_CERT_PASSWORD",
    "APPSTORE_PRIVATE_KEY",
    "SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64",
    "SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64",
    "SPARKLE_STABLE_EDDSA_PRIVATE_KEY_B64",
    "SPARKLE_STABLE_PREVIOUS_EDDSA_PRIVATE_KEY_B64",
    "GRAMDRIVE_API_ID",
    "GRAMDRIVE_API_HASH",
    '"api_hash"',
    '"github_token"',
)


class CandidateError(RuntimeError):
    pass


def sha256_file(path: Path) -> str:
    digest = sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise CandidateError(f"cannot read JSON {path}: {error}") from error
    if not isinstance(value, dict):
        raise CandidateError(f"expected a JSON object in {path}")
    return value


def write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CandidateError(message)


def parse_checksums(path: Path, *, allow_parent_file: bool = False) -> dict[str, str]:
    checksums: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as error:
        raise CandidateError(f"cannot read checksums {path}: {error}") from error
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        if not match:
            raise CandidateError(f"malformed checksum line {path}:{number}")
        digest, name = match.groups()
        require(name not in checksums, f"duplicate checksum entry {name!r} in {path}")
        candidate = Path(name)
        parent_file = (
            allow_parent_file
            and len(candidate.parts) == 2
            and candidate.parts[0] == ".."
            and candidate.parts[1] not in ("", ".", "..")
        )
        require(
            not candidate.is_absolute()
            and not re.match(r"^[A-Za-z]:[\\/]", name)
            and (".." not in candidate.parts or parent_file),
            f"unsafe checksum path {name!r}",
        )
        checksums[name] = digest
    return checksums


def render_checksums(entries: Iterable[tuple[str, str]]) -> str:
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(entries))


def file_inventory(root: Path, *, excluded: Iterable[str] = ()) -> set[str]:
    excluded_set = set(excluded)
    return {
        path.relative_to(root).as_posix()
        for path in root.rglob("*")
        if path.is_file()
        and not path.is_symlink()
        and path.relative_to(root).as_posix() not in excluded_set
    }


def verify_checksum_inventory(
    root: Path,
    checksum_path: Path,
    expected: set[str],
    *,
    allow_parent_file: bool = False,
) -> dict[str, str]:
    """Verify exact inventory membership, containment policy, and every byte."""
    checksums = parse_checksums(checksum_path, allow_parent_file=allow_parent_file)
    missing = sorted(expected - set(checksums))
    extra = sorted(set(checksums) - expected)
    require(not missing and not extra, f"checksum inventory mismatch in {checksum_path.name}: missing={missing}, extra={extra}")
    resolved_root = root.resolve()
    resolved_parent = resolved_root.parent
    for name, digest in checksums.items():
        target = (root / name).resolve()
        allowed_root = resolved_parent if name.startswith("../") else resolved_root
        require(target.is_relative_to(allowed_root), f"checksum path escapes its allowed root: {name!r}")
        require(target.is_file() and not target.is_symlink(), f"checksummed file is missing or a symlink: {name!r}")
        require(sha256_file(target) == digest, f"checksum mismatch: {name}")
    return checksums


def scrub_text_files(root: Path) -> None:
    for path in sorted(root.iterdir()):
        if path.is_file() and path.suffix in TEXT_SUFFIXES:
            text = path.read_text(encoding="utf-8")
            for forbidden in FORBIDDEN_TEXT:
                require(forbidden not in text, f"privacy scrub rejected {path.name}: forbidden credential marker")
            require("/Users/" not in text and "/home/" not in text, f"privacy scrub rejected local path in {path.name}")


def require_portable_tdlib_bytes(path: Path, label: str) -> None:
    """Reject compiled builder defaults even when Mach-O load commands are clean."""

    try:
        payload = path.read_bytes()
    except OSError as error:
        raise CandidateError(f"cannot inspect {label} TDLib bytes: {error}") from error
    offenders = [
        marker.decode("ascii") for marker in FORBIDDEN_RUNTIME_PATH_MARKERS if marker in payload
    ]
    require(
        not offenders,
        f"{label} TDLib contains builder-local OpenSSL/runtime paths despite clean "
        "Mach-O linkage: " + ", ".join(offenders),
    )


def _accepted_notarization(app: dict) -> bool:
    note = app.get("notarization", {})
    return (
        note.get("submitted") is True
        and note.get("status") == "Accepted"
        and note.get("app", {}).get("status") == "Accepted"
        and note.get("dmg", {}).get("status") == "Accepted"
    )


def validate_inputs(
    *,
    app_dir: Path,
    core_dir: Path,
    tdlib_dir: Path,
    mode: str,
    commit: str,
    minimum_build: int,
) -> tuple[dict, dict, dict, Path, str]:
    require(mode in MODE_TO_CHANNEL, f"unsupported candidate mode {mode!r}")
    require(COMMIT_RE.fullmatch(commit) is not None, "commit must be a full lowercase SHA-1")
    app = read_json(app_dir / "manifest.json")
    core = read_json(core_dir / "manifest.json")
    tdlib = read_json(tdlib_dir / "manifest.json")

    require(app.get("schema") == 1 and app.get("signed") is True, "app manifest is not a signed schema-1 artifact")
    require(app.get("git", {}).get("commit") == commit, "app commit does not match requested source")
    require(app.get("git", {}).get("worktree_clean") is True, "app was built from a dirty worktree")
    require(app.get("platform") == "macos-arm64", "app platform is not macos-arm64")
    require(app.get("binary_arch", {}).get("required") == "arm64", "app architecture gate did not require arm64")
    require(app.get("sparkle", {}).get("channel") == MODE_TO_CHANNEL[mode], "embedded Sparkle channel does not match candidate mode")
    require(app.get("team_id") == TEAM_ID and app.get("signing_identity") == IDENTITY, "unexpected Developer ID identity")
    require({item.get("bundle_id") for item in app.get("binaries", [])} == EXPECTED_BUNDLE_IDS, "bundle identity set is incomplete or unexpected")
    require(all(item.get("cdhash") for item in app.get("binaries", [])), "nested signed binary is missing a cdhash")
    require(_accepted_notarization(app), "app and DMG must both have Accepted notarization records")
    require(app.get("signature_verification") == {"app": "passed", "dmg": "passed", "nested": "passed"}, "signature verification is incomplete")
    require(app.get("staple_verification") == {"app": "validated", "dmg": "validated"}, "staple verification is incomplete")
    require(app.get("gatekeeper", {}).get("app") == "accepted" and app.get("gatekeeper", {}).get("dmg") == "accepted", "Gatekeeper did not accept both app and DMG")
    shipped_code = app.get("shipped_code_verification", {})
    shipped_objects = shipped_code.get("objects", [])
    require(shipped_code.get("complete") is True, "shipped code readback is incomplete")
    require(shipped_code.get("required_architecture") == "arm64", "shipped code readback used the wrong architecture policy")
    require(shipped_code.get("expected_team_id") == TEAM_ID, "shipped code readback used the wrong Team ID policy")
    require(shipped_code.get("expected_authority") == IDENTITY, "shipped code readback used the wrong signing authority policy")
    require(isinstance(shipped_objects, list) and shipped_code.get("count") == len(shipped_objects) and shipped_objects, "shipped code readback inventory is empty or inconsistent")
    shipped_paths = [item.get("path") for item in shipped_objects if isinstance(item, dict)]
    require(len(shipped_paths) == len(shipped_objects) and len(set(shipped_paths)) == len(shipped_paths), "shipped code readback paths are invalid or duplicated")
    require(
        all(
            isinstance(path, str)
            and not Path(path).is_absolute()
            and ".." not in Path(path).parts
            for path in shipped_paths
        ),
        "shipped code readback contains an unsafe path",
    )
    require(
        all(
            item.get("signature") == "passed"
            and "arm64" in item.get("architectures", [])
            and item.get("team_id") == TEAM_ID
            and item.get("authority") == IDENTITY
            for item in shipped_objects
        ),
        "a shipped code object failed architecture, signature, Team ID, or authority readback",
    )
    require(app.get("tdjson", {}).get("linked") is True, "app does not carry a live TDJSON-linked core")
    require(app.get("tdjson", {}).get("embedded_libraries") == ["libtdjson.dylib"], "app runtime payload is not the hermetic libtdjson-only contract")
    app_runtime = app.get("tdjson", {}).get("runtime", {})
    app_trust_store = app_runtime.get("trust_store", {})
    require(
        app_runtime.get("verified") is True
        and app_runtime.get("dependency_policy")
        == "system-or-bundle-relative-static-openssl"
        and app_runtime.get("openssl_linkage") == "static",
        "app runtime dependency closure was not verified as portable static-OpenSSL",
    )
    require(
        app_runtime.get("forbidden_builder_paths_verified") is True
        and app_trust_store.get("policy") == "macos-system-pem"
        and app_trust_store.get("cert_file") == OPENSSL_CERT_FILE
        and app_trust_store.get("environment_overrides_scrubbed") is True
        and app_trust_store.get("verified") is True
        and isinstance(app_trust_store.get("certificate_objects"), int)
        and app_trust_store.get("certificate_objects", 0) > 0,
        "app portable macOS trust-store proof is missing or malformed",
    )

    version = app.get("product_version", {})
    build_text = str(version.get("build", ""))
    require(
        re.fullmatch(r"[1-9][0-9]*", build_text) is not None
        and int(build_text) > minimum_build,
        "candidate build is not newer than the applicable feed",
    )
    git_floor_text = str(version.get("git_build_floor", ""))
    require(
        re.fullmatch(r"[1-9][0-9]*", git_floor_text) is not None
        and int(build_text) >= int(git_floor_text),
        "candidate build does not satisfy its git-derived floor",
    )
    build_source = version.get("build_source")
    require(
        build_source == "reviewed-workflow-override"
        or (build_source == "git-revision-count" and build_text == git_floor_text),
        "candidate build source does not match its git-derived floor",
    )
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", str(version.get("short", ""))) is not None, "marketing version is not three-component")

    require(core.get("git", {}).get("commit") == commit, "core commit does not match requested source")
    require(core.get("git", {}).get("worktree_clean") is True, "core was built from a dirty worktree")
    require(core.get("tdjson", {}).get("linked") is True, "core manifest is not live TDJSON-linked")
    core_runtime = core.get("tdjson", {}).get("runtime", {})
    require(core.get("host_test_slice") is None, "candidate core contains a host-test-only slice")
    tdlib_builder_commit = str(tdlib.get("gramdrive", {}).get("commit", ""))
    require(COMMIT_RE.fullmatch(tdlib_builder_commit) is not None, "TDLib builder commit is missing")
    require(tdlib.get("gramdrive", {}).get("worktree_clean") is True, "TDLib artifact came from a dirty GramDrive worktree")
    require(
        tdlib.get("tdlib", {}).get("repo") == PINNED_TDLIB_REPO
        and tdlib.get("tdlib", {}).get("commit") == PINNED_TDLIB_COMMIT,
        "TDLib source identity is not pinned",
    )
    require(tdlib.get("target", {}).get("label") == "macos-arm64" and tdlib.get("target", {}).get("arch") == "arm64", "TDLib target is not macos-arm64")
    require(tdlib.get("reproducibility", {}).get("clean_build_tree") is True, "TDLib did not use a clean build tree")
    tdlib_runtime = tdlib.get("runtime", {})
    tdlib_trust_store = tdlib_runtime.get("trust_store", {})
    require(
        tdlib_runtime.get("dependency_policy") == "system-only-static-openssl"
        and tdlib_runtime.get("openssl_linkage") == "static"
        and tdlib.get("linkage") == tdlib_runtime.get("dependencies"),
        "TDLib runtime dependency provenance is not hermetic static-OpenSSL",
    )
    require(
        tdlib_runtime.get("forbidden_builder_paths_verified") is True
        and tdlib_trust_store.get("policy") == "macos-system-pem"
        and tdlib_trust_store.get("cert_file") == OPENSSL_CERT_FILE
        and tdlib_trust_store.get("environment_overrides_scrubbed") is True
        and tdlib_trust_store.get("verified") is True
        and isinstance(tdlib_trust_store.get("certificate_objects"), int)
        and tdlib_trust_store.get("certificate_objects", 0) > 0
        and core_runtime == tdlib_runtime
        and app_trust_store == tdlib_trust_store,
        "TDLib portable trust-store proof is missing or was not preserved through core/app",
    )
    tdlib_openssl = tdlib.get("third_party", {}).get("openssl", {})
    tdlib_openssl_license = tdlib_openssl.get("license", {})
    require(
        tdlib_openssl.get("name") == "OpenSSL"
        and re.fullmatch(
            r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?",
            str(tdlib_openssl.get("version", "")),
        )
        is not None
        and tdlib_openssl.get("linkage") == "static"
        and tdlib_openssl.get("embedded_in") == "lib/libtdjson.dylib"
        and tdlib_openssl.get("version") == PINNED_OPENSSL_VERSION
        and tdlib_openssl.get("source", {}).get("sha256")
        == PINNED_OPENSSL_SOURCE_SHA256
        and tdlib_openssl_license.get("id") == "Apache-2.0"
        and tdlib_openssl_license.get("file") == OPENSSL_LICENSE_PATH
        and SHA256_RE.fullmatch(str(tdlib_openssl_license.get("sha256", "")))
        is not None,
        "TDLib static OpenSSL attribution is missing or malformed",
    )
    core_openssl = core.get("third_party", {}).get("openssl", {})
    app_openssl = app.get("third_party", {}).get("openssl", {})
    for label, record, license_path in (
        ("core", core_openssl, OPENSSL_LICENSE_PATH),
        ("app", app_openssl, f"Contents/Resources/{OPENSSL_LICENSE_PATH}"),
    ):
        require(
            record.get("name") == tdlib_openssl.get("name")
            and record.get("version") == tdlib_openssl.get("version")
            and record.get("linkage") == "static"
            and record.get("embedded_in") == tdlib_openssl.get("embedded_in")
            and record.get("source") == tdlib_openssl.get("source")
            and record.get("build_options") == tdlib_openssl.get("build_options")
            and record.get("license", {}).get("id") == "Apache-2.0"
            and record.get("license", {}).get("file") == license_path
            and record.get("license", {}).get("sha256")
            == tdlib_openssl_license.get("sha256"),
            f"{label} OpenSSL attribution does not match the TDLib artifact",
        )
    tdlib_digest = tdlib.get("artifacts", {}).get("library", {}).get("sha256")
    require(SHA256_RE.fullmatch(str(tdlib_digest or "")) is not None, "TDLib library digest is missing")
    require(core.get("tdjson", {}).get("library_sha256") == tdlib_digest, "core is not linked to the staged TDLib bytes")

    dmgs = sorted(app_dir.glob("GramDrive-*.dmg"))
    require(len(dmgs) == 1, "expected exactly one candidate DMG")
    dmg = dmgs[0]
    dmg_digest = sha256_file(dmg)
    app_bundle = app_dir / "GramDrive.app"
    require(app_bundle.is_dir(), "packaged GramDrive.app is missing")
    app_expected = {dmg.name} | {
        f"GramDrive.app/{name}" for name in file_inventory(app_bundle)
    }
    app_checksums = verify_checksum_inventory(
        app_dir,
        app_dir / "CHECKSUMS.sha256",
        app_expected,
    )
    core_artifact = core_dir / "GramDriveCore"
    require(core_artifact.is_dir(), "staged GramDriveCore artifact is missing")
    core_zips = sorted(core_dir.glob("GramDriveCore-*.zip"))
    require(len(core_zips) == 1, "expected exactly one staged GramDriveCore zip")
    core_expected = file_inventory(core_artifact) | {f"../{core_zips[0].name}"}
    core_checksums = verify_checksum_inventory(
        core_artifact,
        core_dir / "CHECKSUMS.sha256",
        core_expected,
        allow_parent_file=True,
    )
    tdlib_expected = file_inventory(tdlib_dir, excluded={"manifest.json", "CHECKSUMS.sha256"})
    tdlib_checksums = verify_checksum_inventory(
        tdlib_dir,
        tdlib_dir / "CHECKSUMS.sha256",
        tdlib_expected,
    )
    require(app_checksums.get(dmg.name) == dmg_digest, "DMG checksum does not match app packaging record")
    require(
        core_checksums.get("lib/libtdjson.dylib") == tdlib_digest,
        "staged core TDLib bytes do not match the pinned TDLib artifact",
    )
    transition = app.get("tdjson", {}).get("signing_transition", {})
    shipped_tdlib_digest = app_checksums.get(
        "GramDrive.app/Contents/Frameworks/libtdjson.dylib"
    )
    require(
        transition.get("required") is True
        and transition.get("operation") == "developer-id-codesign"
        and transition.get("source")
        == {"artifact": "staged-core", "sha256": tdlib_digest}
        and transition.get("pre_sign")
        == {
            "bundle_path": "Contents/Frameworks/libtdjson.dylib",
            "sha256": tdlib_digest,
            "matches_source": True,
        },
        "TDLib pre-sign provenance does not match the authoritative staged bytes",
    )
    require(
        SHA256_RE.fullmatch(str(shipped_tdlib_digest or "")) is not None
        and transition.get("post_sign")
        == {
            "bundle_path": "Contents/Frameworks/libtdjson.dylib",
            "sha256": shipped_tdlib_digest,
        },
        "TDLib post-sign provenance does not match the final shipped bytes",
    )
    require(
        transition.get("signature")
        == {
            "verified": True,
            "team_id": TEAM_ID,
            "authority": IDENTITY,
            "architecture": "arm64",
        },
        "TDLib signed transition lacks strict Developer ID signature readback",
    )
    tdlib_shipped_objects = [
        item
        for item in shipped_objects
        if item.get("path") == "Contents/Frameworks/libtdjson.dylib"
    ]
    require(
        len(tdlib_shipped_objects) == 1
        and tdlib_shipped_objects[0].get("signature") == "passed"
        and tdlib_shipped_objects[0].get("team_id")
        == transition["signature"]["team_id"]
        and tdlib_shipped_objects[0].get("authority")
        == transition["signature"]["authority"]
        and transition["signature"]["architecture"]
        in tdlib_shipped_objects[0].get("architectures", []),
        "TDLib signed transition is not backed by exactly one shipped-code readback",
    )
    require(app.get("checksums") == app_checksums, "app manifest checksum inventory does not match CHECKSUMS.sha256")
    require(core.get("checksums") == core_checksums, "core manifest checksum inventory does not match CHECKSUMS.sha256")
    tdlib_manifest_checksums = {
        name: record.get("sha256")
        for name, record in tdlib.get("artifacts", {}).get("files", {}).items()
        if isinstance(record, dict)
    }
    require(tdlib_manifest_checksums == tdlib_checksums, "TDLib manifest checksum inventory does not match CHECKSUMS.sha256")
    openssl_license_digest = str(tdlib_openssl_license["sha256"])
    require(
        tdlib_checksums.get(OPENSSL_LICENSE_PATH) == openssl_license_digest
        and core_checksums.get(OPENSSL_LICENSE_PATH) == openssl_license_digest
        and app_checksums.get(APP_OPENSSL_LICENSE_PATH) == openssl_license_digest,
        "OpenSSL license bytes are not identical across TDLib, core, and signed app inventories",
    )
    require(app.get("checksums", {}).get(dmg.name) == dmg_digest, "DMG checksum does not match app manifest")
    require(app.get("sizes", {}).get("dmg_bytes") == dmg.stat().st_size, "DMG size does not match app manifest")
    for label, library in (
        ("authoritative", tdlib_dir / "lib" / "libtdjson.dylib"),
        ("core", core_artifact / "lib" / "libtdjson.dylib"),
        ("signed app", app_bundle / "Contents" / "Frameworks" / "libtdjson.dylib"),
    ):
        require_portable_tdlib_bytes(library, label)
    return app, core, tdlib, dmg, dmg_digest


def build_candidate(args: argparse.Namespace) -> dict:
    app_dir, core_dir, tdlib_dir, out_dir = (Path(value).resolve() for value in (args.app_dir, args.core_dir, args.tdlib_dir, args.out_dir))
    app, core, tdlib, dmg, dmg_digest = validate_inputs(
        app_dir=app_dir,
        core_dir=core_dir,
        tdlib_dir=tdlib_dir,
        mode=args.mode,
        commit=args.commit,
        minimum_build=args.minimum_build,
    )
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    candidate_dmg_name = f"GramDrive-{app['product_version']['short']}-{app['product_version']['build']}.dmg"
    copied_dmg = out_dir / candidate_dmg_name
    shutil.copyfile(dmg, copied_dmg)
    require(sha256_file(copied_dmg) == dmg_digest, "copied candidate DMG bytes changed")
    source_files = {
        "app-manifest.json": app_dir / "manifest.json",
        "app-checksums.sha256": app_dir / "CHECKSUMS.sha256",
        "core-manifest.json": core_dir / "manifest.json",
        "core-checksums.sha256": core_dir / "CHECKSUMS.sha256",
        "tdlib-manifest.json": tdlib_dir / "manifest.json",
        "tdlib-checksums.sha256": tdlib_dir / "CHECKSUMS.sha256",
    }
    for name, source in source_files.items():
        require(source.is_file(), f"required provenance file is missing: {source}")
        shutil.copyfile(source, out_dir / name)

    verification = {
        "schema": SCHEMA,
        "result": "passed",
        "gates": {
            "architecture": "passed",
            "bundle_and_team_identity": "passed",
            "checksums": "passed",
            "gatekeeper_app_and_dmg": "passed",
            "live_tdlib": "passed",
            "live_tdlib_signed_transition": "passed",
            "manifest": "passed",
            "nested_and_dmg_signatures": "passed",
            "notarization_app_and_dmg": "passed",
            "privacy_scrub": "passed",
            "staples_app_and_dmg": "passed",
        },
    }
    provenance = {
        "schema": SCHEMA,
        "source": {"repository": args.repository, "commit": args.commit, "ref": args.ref},
        "workflow": {"ref": args.workflow_ref, "run_id": args.run_id, "run_attempt": args.run_attempt},
        "candidate": {"mode": args.mode, "channel": MODE_TO_CHANNEL[args.mode]},
        "toolchains": {"app": app.get("toolchain", {}), "core": core.get("toolchain", {}), "tdlib": tdlib.get("toolchain", {})},
        "tdlib": {
            "repo": tdlib.get("tdlib", {}).get("repo"),
            "commit": tdlib.get("tdlib", {}).get("commit"),
            "version": tdlib.get("tdlib", {}).get("runtime_version"),
            "library_sha256": tdlib.get("artifacts", {}).get("library", {}).get("sha256"),
            "builder_source_commit": tdlib.get("gramdrive", {}).get("commit"),
            "runtime": tdlib.get("runtime"),
            "third_party": tdlib.get("third_party"),
        },
        "app_runtime": app.get("tdjson", {}).get("runtime"),
        "app_tdlib_signing_transition": app.get("tdjson", {}).get(
            "signing_transition"
        ),
        "app_third_party": app.get("third_party"),
    }
    write_json(out_dir / "verification.json", verification)
    write_json(out_dir / "candidate-provenance.json", provenance)

    manifest = {
        "schema": SCHEMA,
        "name": "GramDrive candidate",
        "source": provenance["source"],
        "workflow": provenance["workflow"],
        "mode": args.mode,
        "channel": MODE_TO_CHANNEL[args.mode],
        "version": app["product_version"],
        "identity": {"signing": app["signing_identity"], "team_id": app["team_id"], "bundle_ids": sorted(EXPECTED_BUNDLE_IDS)},
        "dmg": {
            "name": candidate_dmg_name,
            "packaging_name": dmg.name,
            "sha256": dmg_digest,
            "bytes": dmg.stat().st_size,
        },
        "attestation": {"file": "candidate-attestation.json", "status": "required-before-upload"},
        "publication": {"owned": False, "downstream_task": "TASK-260810-y3zcg8"},
    }
    write_json(out_dir / "candidate-manifest.json", manifest)

    subject_names = sorted(path.name for path in out_dir.iterdir() if path.is_file())
    (out_dir / "SUBJECTS.sha256").write_text(
        render_checksums((name, sha256_file(out_dir / name)) for name in subject_names), encoding="utf-8"
    )
    scrub_text_files(out_dir)
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            output.write(f"build={app['product_version']['build']}\n")
            output.write(f"dmg_name={candidate_dmg_name}\n")
            output.write(f"dmg_sha256={dmg_digest}\n")
    return manifest


def attestation_subjects(bundle: dict) -> dict[str, str]:
    try:
        payload = bundle["dsseEnvelope"]["payload"]
        statement = json.loads(base64.b64decode(payload, validate=True))
        subjects = statement["subject"]
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise CandidateError(f"invalid Sigstore attestation bundle: {error}") from error
    result: dict[str, str] = {}
    for subject in subjects:
        name = subject.get("name")
        digest = subject.get("digest", {}).get("sha256")
        require(isinstance(name, str) and SHA256_RE.fullmatch(str(digest or "")) is not None, "invalid attestation subject")
        basename = Path(name).name
        require(name == basename, f"attestation subject must be a package-root filename: {name!r}")
        require(basename not in result, f"duplicate attestation subject basename {basename!r}")
        result[basename] = digest
    return result


def finalize_candidate(args: argparse.Namespace) -> dict:
    out_dir = Path(args.out_dir).resolve()
    bundle = read_json(Path(args.attestation_bundle).resolve())
    expected = parse_checksums(out_dir / "SUBJECTS.sha256")
    require(attestation_subjects(bundle) == expected, "attestation subjects do not exactly match candidate SUBJECTS.sha256")
    write_json(out_dir / "candidate-attestation.json", bundle)
    manifest = read_json(out_dir / "candidate-manifest.json")
    write_json(
        out_dir / "finalization.json",
        {
            "schema": SCHEMA,
            "status": "verified-and-attested",
            "attestation": {
                "file": "candidate-attestation.json",
                "sha256": sha256_file(out_dir / "candidate-attestation.json"),
                "subjects": "SUBJECTS.sha256",
            },
            "privacy_scrub": "passed",
        },
    )
    scrub_text_files(out_dir)
    names = sorted(path.name for path in out_dir.iterdir() if path.is_file() and path.name != "CANDIDATE-CHECKSUMS.sha256")
    (out_dir / "CANDIDATE-CHECKSUMS.sha256").write_text(
        render_checksums((name, sha256_file(out_dir / name)) for name in names), encoding="utf-8"
    )
    return manifest


def verify_candidate(args: argparse.Namespace) -> dict:
    out_dir = Path(args.out_dir).resolve()
    checksums = parse_checksums(out_dir / "CANDIDATE-CHECKSUMS.sha256")
    actual_names = {path.name for path in out_dir.iterdir() if path.is_file() and path.name != "CANDIDATE-CHECKSUMS.sha256"}
    require(set(checksums) == actual_names, "candidate checksum inventory does not exactly match package files")
    for name, digest in checksums.items():
        require(sha256_file(out_dir / name) == digest, f"candidate file checksum mismatch: {name}")
    manifest = read_json(out_dir / "candidate-manifest.json")
    finalization = read_json(out_dir / "finalization.json")
    require(finalization.get("status") == "verified-and-attested", "candidate is not finalized")
    require(
        sha256_file(out_dir / "candidate-attestation.json") == finalization.get("attestation", {}).get("sha256"),
        "candidate attestation digest does not match finalization record",
    )
    require(sha256_file(out_dir / manifest["dmg"]["name"]) == manifest["dmg"]["sha256"], "exact DMG digest does not match candidate manifest")
    expected = parse_checksums(out_dir / "SUBJECTS.sha256")
    for name, digest in expected.items():
        subject = out_dir / name
        require(subject.is_file(), f"attested candidate subject is missing: {name}")
        require(sha256_file(subject) == digest, f"attested candidate subject changed: {name}")
    require(attestation_subjects(read_json(out_dir / "candidate-attestation.json")) == expected, "candidate attestation subjects changed")
    scrub_text_files(out_dir)
    return manifest


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build")
    build.add_argument("--app-dir", required=True)
    build.add_argument("--core-dir", required=True)
    build.add_argument("--tdlib-dir", required=True)
    build.add_argument("--out-dir", required=True)
    build.add_argument("--mode", choices=tuple(MODE_TO_CHANNEL), required=True)
    build.add_argument("--commit", required=True)
    build.add_argument("--minimum-build", type=int, default=0)
    build.add_argument("--repository", required=True)
    build.add_argument("--ref", required=True)
    build.add_argument("--workflow-ref", required=True)
    build.add_argument("--run-id", required=True)
    build.add_argument("--run-attempt", required=True)
    finalize = commands.add_parser("finalize")
    finalize.add_argument("--out-dir", required=True)
    finalize.add_argument("--attestation-bundle", required=True)
    verify = commands.add_parser("verify")
    verify.add_argument("--out-dir", required=True)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "build":
            manifest = build_candidate(args)
        elif args.command == "finalize":
            manifest = finalize_candidate(args)
        else:
            manifest = verify_candidate(args)
    except CandidateError as error:
        print(f"CANDIDATE PACKAGE FAILED: {error}", file=sys.stderr)
        return 1
    print(f"CANDIDATE PACKAGE {args.command.upper()} PASSED: {manifest['dmg']['name']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
