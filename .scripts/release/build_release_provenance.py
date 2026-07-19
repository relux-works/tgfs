#!/usr/bin/env python3
"""Build the release provenance bundle for a GramDrive release.

The signed, notarized artifact itself is produced by
`.scripts/apple-app/build_app_bundle.py` (TASK-260715-1dk9ik). This script is the
*release* side (TASK-260715-3bhbkv): given that packaged output — its
`manifest.json`, the `.dmg`, and `CHECKSUMS.sha256` — it derives the provenance a
release needs to be independently verifiable, credential-free, and traceable to a
reviewed commit (the task AC):

    .temp/release/
      sbom.json              CycloneDX 1.5 dependency inventory (POL-6)
      CHANGELOG.md           the commits this release introduces since the last tag
      rollback.json          what this release is and the release to fall back to
      release-manifest.json  ties tag/version/commit to every artifact by sha256
      RELEASE-CHECKSUMS.sha256  sha256 of every provenance file above

The version, commit, notarization status and toolchain are *read from the app
manifest*, not recomputed — the release record must describe the artifact that
was actually signed, not a second independent guess at it. The SBOM is built from
`cargo metadata` (every third-party crate carries its own `license` field) and
SwiftPM's `Package.resolved`; the Apple support package has no third-party SPM
dependencies (GramDriveCore is resolved by path), so that side is normally empty.

POL-6 enforcement is *not* re-implemented here. The permissive-license/advisory
gate is `cargo deny check`, which the core CI suite already runs (deny.toml). A
second policy engine in this script would be a second source of truth that drifts
from deny.toml the moment either is edited; the SBOM records each license
verbatim and points at the gate, it does not re-adjudicate it.

Nothing here reads or writes a credential. As a backstop, the final step scans
every produced file for secret-shaped content and fails the run if it finds any
(the task AC: "contain no development credentials/sessions").

Usage:
    build_release_provenance.py --package-dir .temp/app-packaging --out-dir .temp/release
    build_release_provenance.py --package-dir .temp/app-packaging --tag v1.2.0

This is a reusable script the tag-triggered `.github/workflows/release.yml`
invokes; it is not a second copy of the release steps. It needs no network, no
signing identity and no Xcode — only `git` and `cargo` — so `make
release-provenance` is a clean local dry-run of every non-signing release step.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import uuid
from collections.abc import Callable, Sequence
from datetime import UTC, datetime
from hashlib import sha256
from pathlib import Path

# Default locations, both under the gitignored .temp tree.
PACKAGE_DIR = Path(".temp/app-packaging")
OUT_DIR = Path(".temp/release")

PRODUCT_NAME = "GramDrive"

# The Apple support package. It resolves GramDriveCore by path and declares no
# other dependency, so its Package.resolved (when one exists) lists only pins for
# third-party SPM packages — of which there are none today.
SWIFT_PACKAGE = Path("apple/GramDriveSupport")

# The tool line recorded in the SBOM and the release manifest.
TOOL_NAME = "build_release_provenance.py"
TOOL_VERSION = "1"

# The gate that actually enforces POL-6; recorded so a reader of the SBOM knows
# where license/advisory adjudication happens rather than looking for it here.
LICENSE_GATE = "cargo deny check (deny.toml [licenses] + [advisories]); run by the core CI suite"


# -- process boundary --------------------------------------------------------

Runner = Callable[[Sequence[str], Path], "tuple[int, str]"]


def default_runner(argv: Sequence[str], cwd: Path) -> tuple[int, str]:
    """Run argv in cwd, returning (exit code, combined output)."""
    import subprocess

    try:
        proc = subprocess.run(list(argv), cwd=cwd, capture_output=True, text=True)
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found on PATH\n"
    return proc.returncode, proc.stdout + proc.stderr


class ReleaseError(Exception):
    """A release-provenance step failed; carries a readable reason."""


# -- hashing -----------------------------------------------------------------


def sha256_bytes(data: bytes) -> str:
    return sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def format_checksums(checksums: dict[str, str]) -> str:
    """Render checksums in `shasum -a 256 -c` format, same shape the packaging
    pipeline uses so one tool verifies every artifact."""
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items()))


# -- git ---------------------------------------------------------------------


def git(runner: Runner, repo_root: Path, *args: str) -> tuple[int, str]:
    return runner(("git", *args), repo_root)


def resolve_commit(runner: Runner, repo_root: Path, ref: str) -> str | None:
    code, out = git(runner, repo_root, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
    return out.strip() if code == 0 and out.strip() else None


def previous_tag(runner: Runner, repo_root: Path, tag: str) -> str | None:
    """The most recent tag strictly before `tag` (the release we roll back to).

    `git describe --tags --abbrev=0 <tag>^` walks back from the release's parent
    to the nearest earlier tag. Returns None for the very first release, where
    there is nothing earlier and the changelog is the whole history.
    """
    base = tag if resolve_commit(runner, repo_root, tag) else "HEAD"
    code, out = git(runner, repo_root, "describe", "--tags", "--abbrev=0", f"{base}^")
    return out.strip() if code == 0 and out.strip() else None


def commit_log(runner: Runner, repo_root: Path, rev_range: str) -> list[str]:
    """`<from>..<to>` one-line commits (subject + short hash + author), newest
    first — the changelog body."""
    code, out = git(
        runner, repo_root, "log", "--no-merges", "--pretty=format:%h %s (%an)", rev_range
    )
    if code != 0:
        return []
    return [line.rstrip() for line in out.splitlines() if line.strip()]


# -- version, read from the app manifest -------------------------------------


def load_app_manifest(package_dir: Path) -> dict:
    """The signed artifact's own record; the release provenance is derived from
    it so both describe the same bytes."""
    path = package_dir / "manifest.json"
    if not path.is_file():
        raise ReleaseError(
            f"app manifest not found at {path}; run `make package-app` (or the "
            f"release workflow's packaging step) first"
        )
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ReleaseError(f"app manifest at {path} is not valid JSON: {exc}") from exc


def parse_checksums(text: str) -> dict[str, str]:
    """Parse a `shasum -a 256 -c` file back into {name: digest}."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        digest, _, name = line.partition("  ")
        if name:
            out[name.strip()] = digest.strip()
    return out


# -- SBOM --------------------------------------------------------------------


def cargo_metadata(runner: Runner, repo_root: Path) -> dict:
    """The resolved Cargo dependency graph. `--locked` pins it to Cargo.lock so
    the inventory is exactly what a release build links, `--all-features` matches
    the graph deny.toml (and clippy) evaluate."""
    code, out = runner(
        ("cargo", "metadata", "--format-version", "1", "--all-features", "--locked"),
        repo_root,
    )
    if code != 0:
        raise ReleaseError(f"`cargo metadata` failed (exit {code}):\n{out}")
    try:
        return json.loads(out)
    except json.JSONDecodeError as exc:
        raise ReleaseError(f"`cargo metadata` did not emit JSON: {exc}") from exc


def license_entries(license_value: str | None) -> list[dict]:
    """CycloneDX license form for a crate's `license` field.

    Cargo licenses are SPDX *expressions* ("MIT OR Apache-2.0"), so the
    expression form is used whenever a value is present — a bare id is itself a
    valid expression. A crate with no `license` (some first-party or dual
    license-file crates) yields an empty list, which the caller flags with a
    property rather than silently claiming a license it does not state.
    """
    if not license_value or not license_value.strip():
        return []
    return [{"expression": license_value.strip()}]


def cargo_components(metadata: dict) -> list[dict]:
    """Third-party Cargo crates as CycloneDX components, sorted for determinism.

    "Third-party" = a package with a `source` (registry/git); the workspace's own
    crates (no source) are the product being described, not its dependencies.
    """
    components: list[dict] = []
    for pkg in metadata.get("packages", []):
        if not pkg.get("source"):
            continue
        name = pkg["name"]
        version = pkg["version"]
        licenses = license_entries(pkg.get("license"))
        component: dict = {
            "type": "library",
            "name": name,
            "version": version,
            "purl": f"pkg:cargo/{name}@{version}",
            "licenses": licenses,
        }
        properties = [{"name": "gramdrive:ecosystem", "value": "cargo"}]
        if not licenses:
            # cargo metadata may carry a license_file instead of an SPDX id;
            # record that the license is stated out of band rather than absent.
            file_ref = pkg.get("license_file")
            properties.append(
                {
                    "name": "gramdrive:license-note",
                    "value": f"no SPDX license field; license_file={file_ref}"
                    if file_ref
                    else "no SPDX license field and no license_file — verify manually (POL-6)",
                }
            )
        component["properties"] = properties
        components.append(component)
    components.sort(key=lambda c: (c["name"], c["version"]))
    return components


def swift_resolved_pins(repo_root: Path) -> list[dict]:
    """Third-party SwiftPM pins from Package.resolved, if one exists.

    The Apple support package resolves GramDriveCore by path and declares no
    other dependency, so today there is no Package.resolved and this returns [].
    Written to handle one appearing later (a real SPM dependency added) without a
    code change: v2/v3 `Package.resolved` both key pins under `pins`.
    """
    for candidate in (
        repo_root / SWIFT_PACKAGE / "Package.resolved",
        repo_root / SWIFT_PACKAGE / ".swiftpm" / "Package.resolved",
    ):
        if not candidate.is_file():
            continue
        try:
            data = json.loads(candidate.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            continue
        pins = data.get("pins") or data.get("object", {}).get("pins") or []
        components: list[dict] = []
        for pin in pins:
            identity = pin.get("identity") or pin.get("package") or "unknown"
            state = pin.get("state", {})
            version = state.get("version") or state.get("revision") or "unknown"
            location = pin.get("location") or pin.get("repositoryURL") or ""
            components.append(
                {
                    "type": "library",
                    "name": identity,
                    "version": version,
                    "purl": f"pkg:swift/{identity}@{version}",
                    "licenses": [],
                    "properties": [
                        {"name": "gramdrive:ecosystem", "value": "swiftpm"},
                        {"name": "gramdrive:location", "value": location},
                    ],
                }
            )
        components.sort(key=lambda c: (c["name"], c["version"]))
        return components
    return []


def serial_number(commit: str | None, tag: str) -> str:
    """A deterministic CycloneDX serial: the same commit+tag always yields the
    same urn, so the SBOM is reproducible (uuid5, not a random uuid4)."""
    seed = f"gramdrive-sbom:{commit or 'unknown'}:{tag}"
    return f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, seed)}"


def build_sbom(
    *,
    product_version: dict,
    commit: str | None,
    tag: str,
    cargo_comps: list[dict],
    swift_comps: list[dict],
    source_date: str,
) -> dict:
    components = cargo_comps + swift_comps
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": serial_number(commit, tag),
        "version": 1,
        "metadata": {
            "timestamp": source_date,
            "tools": [{"vendor": PRODUCT_NAME, "name": TOOL_NAME, "version": TOOL_VERSION}],
            "component": {
                "type": "application",
                "name": PRODUCT_NAME,
                "version": product_version.get("short", "0.0.0"),
                "purl": f"pkg:generic/gramdrive@{product_version.get('short', '0.0.0')}",
            },
            "properties": [
                {"name": "gramdrive:commit", "value": commit or "unknown"},
                {"name": "gramdrive:tag", "value": tag},
                {"name": "gramdrive:license-gate", "value": LICENSE_GATE},
                {"name": "gramdrive:cargo-components", "value": str(len(cargo_comps))},
                {"name": "gramdrive:swiftpm-components", "value": str(len(swift_comps))},
            ],
        },
        "components": components,
    }


# -- changelog + rollback ----------------------------------------------------


def build_changelog(
    *, product_version: dict, tag: str, source_date: str, prev_tag: str | None, commits: list[str]
) -> str:
    short = product_version.get("short", "0.0.0")
    build = product_version.get("build", "0")
    header = f"# {PRODUCT_NAME} {short} ({tag})\n\n"
    meta = f"_CFBundleVersion {build} · {source_date}_\n\n"
    span = (
        f"Changes since {prev_tag}:\n\n"
        if prev_tag
        else "First tagged release — full history:\n\n"
    )
    if commits:
        body = "".join(f"- {line}\n" for line in commits)
    else:
        body = "- (no non-merge commits in range)\n"
    return header + meta + span + body


def build_rollback(
    *,
    tag: str,
    commit: str | None,
    product_version: dict,
    prev_tag: str | None,
    prev_commit: str | None,
    dmg_name: str | None,
    dmg_sha256: str | None,
    notarized: bool,
) -> dict:
    return {
        "schema": 1,
        "name": PRODUCT_NAME,
        "tag": tag,
        "commit": commit,
        "version": product_version,
        "artifact": {"dmg": dmg_name, "dmg_sha256": dmg_sha256, "notarized": notarized},
        "previous": {"tag": prev_tag, "commit": prev_commit},
        "rollback": (
            f"Re-publish the {prev_tag} release artifact and point the update feed "
            f"(Sparkle appcast: CFBundleVersion ordering) back at {prev_tag}. "
            f"CFBundleVersion is the monotonic git rev-count, so {prev_tag} sorts "
            f"below {tag} and clients treat it as the current version again."
            if prev_tag
            else "First release — no earlier version to roll back to."
        ),
    }


# -- credential scrub --------------------------------------------------------

# High-confidence secret material: matched in *every* produced file, including
# the free-form changelog, because none of these can occur innocently.
SECRET_MATERIAL = re.compile(
    r"-----BEGIN[ A-Z]*(PRIVATE KEY|CERTIFICATE)-----"
    r"|AuthKey_[A-Za-z0-9]+\.p8"
    r"|[A-Za-z0-9+/]{200,}={0,2}",  # a 200+ char base64 run — an embedded key/p12 blob
)

# Leak words checked only in the *structured* JSON we author (SBOM, manifests),
# which is controlled content that must never carry them. Deliberately NOT run
# over CHANGELOG.md, whose commit subjects may legitimately say "password".
STRUCTURED_LEAK_WORDS = re.compile(
    r"\b(p12|passphrase|api[_-]?hash|api[_-]?id|MACOS_CERT|KEYCHAIN_PASSWORD)\b"
    r"|password\s*[:=]"
    r"|AuthKey",
    re.IGNORECASE,
)

STRUCTURED_SUFFIXES = {".json"}


def scrub_findings(out_dir: Path) -> list[str]:
    """Return one finding per file that looks like it carries a secret.

    Two passes: high-confidence secret *material* over every file, and a
    leak-word pass over the structured JSON only. Empty list == clean.
    """
    findings: list[str] = []
    for path in sorted(out_dir.rglob("*")):
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        rel = path.relative_to(out_dir)
        if SECRET_MATERIAL.search(text):
            findings.append(f"{rel}: matches secret-material pattern")
        if path.suffix in STRUCTURED_SUFFIXES and STRUCTURED_LEAK_WORDS.search(text):
            findings.append(f"{rel}: structured artifact contains a leak-word token")
    return findings


def assert_credential_free(out_dir: Path) -> None:
    findings = scrub_findings(out_dir)
    if findings:
        joined = "\n  ".join(findings)
        raise ReleaseError(
            "credential scrub FAILED — a release artifact looks like it carries a "
            f"secret:\n  {joined}"
        )


# -- attestation status ------------------------------------------------------

# GitHub artifact attestation (actions/attest-build-provenance) is entitled per
# org plan: a private repo needs a paid plan, a public repo gets it free. On a
# private repo without that entitlement the create-attestation API rejects with
# "Feature not available … upgrade the billing plan, or make this repository
# public" (BUG-260720-116eli). The release workflow preflights that entitlement
# and passes the result here so the release record states the provenance gap
# explicitly instead of silently shipping without an attestation.
ATTESTATION_AVAILABLE = "available"
ATTESTATION_UNAVAILABLE = "unavailable"
ATTESTATION_UNKNOWN = "unknown"
ATTESTATION_STATUSES = (ATTESTATION_AVAILABLE, ATTESTATION_UNAVAILABLE, ATTESTATION_UNKNOWN)


def build_attestation_record(status: str) -> dict:
    """The release manifest's `attestation` block.

    `status` is what the release workflow's entitlement preflight found:

      unavailable  the feature is not entitled for this (private) repo — the
                   release ships WITHOUT a GitHub attestation and says so; the
                   notarization ticket, CHECKSUMS/RELEASE-CHECKSUMS and the SBOM
                   carry integrity. Resumes with zero config once the plan
                   enables it or the repo is made public.
      available    the feature is entitled and an attestation was produced for
                   the dmg and this manifest (verify with `gh attestation
                   verify`).
      unknown      status was not determined (e.g. a local `make
                   release-provenance` dry run that never touches the API).
    """
    if status == ATTESTATION_UNAVAILABLE:
        return {
            "available": False,
            "status": "unavailable (private-repo plan)",
            "note": (
                "GitHub artifact attestation is not entitled for this private repo "
                "under the current org plan; this release ships without a build-"
                "provenance attestation. Integrity is carried by the notarization "
                "ticket, CHECKSUMS/RELEASE-CHECKSUMS and the SBOM. Attestation "
                "resumes with zero config once the plan enables it or the repo is "
                "made public."
            ),
        }
    if status == ATTESTATION_AVAILABLE:
        return {
            "available": True,
            "status": "attested",
            "note": (
                "A GitHub build-provenance attestation was produced for the dmg and "
                "this manifest; verify it with `gh attestation verify`."
            ),
        }
    return {
        "available": None,
        "status": "unknown",
        "note": (
            "Attestation entitlement was not determined for this run (e.g. a local "
            "provenance dry run that does not reach the attestation API)."
        ),
    }


# -- release manifest --------------------------------------------------------


def build_release_manifest(
    *,
    tag: str,
    commit: str | None,
    product_version: dict,
    app_manifest: dict,
    artifacts: dict[str, str],
    prev_tag: str | None,
    attestation_status: str,
) -> dict:
    return {
        "schema": 1,
        "name": PRODUCT_NAME,
        "tag": tag,
        "commit": commit,
        "version": product_version,
        "platform": app_manifest.get("platform", "macos-arm64"),
        "signing_identity": app_manifest.get("signing_identity"),
        "team_id": app_manifest.get("team_id"),
        "notarization": app_manifest.get("notarization", {"submitted": False}),
        "gatekeeper": app_manifest.get("gatekeeper", {}),
        "toolchain": app_manifest.get("toolchain", {}),
        "core_contract_version": app_manifest.get("core_contract_version"),
        "license_gate": LICENSE_GATE,
        "previous_tag": prev_tag,
        # Every artifact this release ships or vouches for, by sha256. A verifier
        # needs nothing but this file and the artifacts to confirm the set.
        "artifacts": [
            {"name": name, "sha256": digest} for name, digest in sorted(artifacts.items())
        ],
        "attestation": build_attestation_record(attestation_status),
        "reproducible": {
            "byte_identical": False,
            "attributable": True,
            "note": (
                "The signed .dmg embeds a per-signature trusted timestamp and is not "
                "byte-reproducible by design (see the app manifest). This release "
                "manifest, its checksums and the SBOM make the release attributable to "
                "a commit and its dependency set (NFR-052), which is what independent "
                "verification requires."
            ),
        },
        "credential_scrub": "passed",
    }


# -- orchestration -----------------------------------------------------------


def source_date_value(app_manifest: dict, environ: dict[str, str]) -> str:
    """SOURCE_DATE_EPOCH if set, else the app manifest's recorded source_date —
    the source's date, never the wall clock (which would break reproducibility)."""
    epoch = environ.get("SOURCE_DATE_EPOCH")
    if epoch and epoch.strip().isdigit():
        return datetime.fromtimestamp(int(epoch.strip()), tz=UTC).isoformat()
    return str(app_manifest.get("source_date", app_manifest.get("git", {}).get("commit", "unknown")))


def write_json(path: Path, data: dict) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=False) + "\n", encoding="utf-8")


def generate(
    repo_root: Path,
    *,
    package_dir: Path,
    out_dir: Path,
    tag: str | None,
    attestation_status: str = ATTESTATION_UNKNOWN,
    runner: Runner = default_runner,
    echo: Callable[[str], None] = print,
    environ: dict[str, str] | None = None,
) -> dict:
    """Produce the whole provenance bundle and return the release manifest."""
    environ = environ if environ is not None else dict(os.environ)
    if attestation_status not in ATTESTATION_STATUSES:
        raise ReleaseError(
            f"unknown attestation status {attestation_status!r}; "
            f"expected one of {', '.join(ATTESTATION_STATUSES)}"
        )

    app_manifest = load_app_manifest(package_dir)
    product_version = app_manifest.get("product_version", {"short": "0.0.0", "build": "0"})
    git_info = app_manifest.get("git", {})
    commit = git_info.get("commit")
    # The tag names the release; fall back to the artifact's own describe when a
    # dry-run has no tag yet (marketing version stays 0.0.0 until a v* tag exists
    # — the packaging review's second note, honoured by reading it from here).
    effective_tag = tag or git_info.get("describe") or "untagged"

    source_date = source_date_value(app_manifest, environ)

    if out_dir.exists():
        import shutil

        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    # --- SBOM ---
    cargo_comps = cargo_components(cargo_metadata(runner, repo_root))
    swift_comps = swift_resolved_pins(repo_root)
    sbom = build_sbom(
        product_version=product_version,
        commit=commit,
        tag=effective_tag,
        cargo_comps=cargo_comps,
        swift_comps=swift_comps,
        source_date=source_date,
    )
    write_json(out_dir / "sbom.json", sbom)

    # --- changelog + rollback ---
    prev_tag = previous_tag(runner, repo_root, effective_tag)
    prev_commit = resolve_commit(runner, repo_root, prev_tag) if prev_tag else None
    rev_range = f"{prev_tag}..HEAD" if prev_tag else "HEAD"
    commits = commit_log(runner, repo_root, rev_range)
    changelog = build_changelog(
        product_version=product_version,
        tag=effective_tag,
        source_date=source_date,
        prev_tag=prev_tag,
        commits=commits,
    )
    (out_dir / "CHANGELOG.md").write_text(changelog, encoding="utf-8")

    checksums = parse_checksums((package_dir / "CHECKSUMS.sha256").read_text(encoding="utf-8")) \
        if (package_dir / "CHECKSUMS.sha256").is_file() \
        else app_manifest.get("checksums", {})
    dmg_name = next((n for n in checksums if n.endswith(".dmg")), None)
    dmg_sha256 = checksums.get(dmg_name) if dmg_name else None
    notarized = bool(app_manifest.get("notarization", {}).get("submitted"))

    rollback = build_rollback(
        tag=effective_tag,
        commit=commit,
        product_version=product_version,
        prev_tag=prev_tag,
        prev_commit=prev_commit,
        dmg_name=dmg_name,
        dmg_sha256=dmg_sha256,
        notarized=notarized,
    )
    write_json(out_dir / "rollback.json", rollback)

    # --- release manifest: ties tag/version/commit to every artifact ---
    # It covers the shipped artifacts (the dmg, from the app checksums) and the
    # provenance files just written, each by its own sha256.
    artifacts: dict[str, str] = {}
    if dmg_name and dmg_sha256:
        artifacts[dmg_name] = dmg_sha256
    for name in ("sbom.json", "CHANGELOG.md", "rollback.json"):
        artifacts[name] = sha256_file(out_dir / name)

    release_manifest = build_release_manifest(
        tag=effective_tag,
        commit=commit,
        product_version=product_version,
        app_manifest=app_manifest,
        artifacts=artifacts,
        prev_tag=prev_tag,
        attestation_status=attestation_status,
    )
    write_json(out_dir / "release-manifest.json", release_manifest)

    # --- checksums over every provenance file, then the scrub ---
    provenance_checksums = {
        name: sha256_file(out_dir / name)
        for name in ("sbom.json", "CHANGELOG.md", "rollback.json", "release-manifest.json")
    }
    (out_dir / "RELEASE-CHECKSUMS.sha256").write_text(
        format_checksums(provenance_checksums), encoding="utf-8"
    )

    # The backstop: nothing produced may look like a credential (task AC).
    assert_credential_free(out_dir)

    echo("")
    echo(f"release:      {effective_tag} ({product_version.get('short')} / {product_version.get('build')})")
    echo(f"commit:       {commit}")
    echo(f"sbom:         {len(cargo_comps)} cargo + {len(swift_comps)} swiftpm components")
    echo(f"changelog:    {len(commits)} commits since {prev_tag or '(first release)'}")
    echo(f"dmg:          {dmg_name or '(none — dry run / unsigned)'}")
    echo(f"notarized:    {notarized}")
    echo(f"attestation:  {release_manifest['attestation']['status']}")
    echo(f"scrub:        passed")
    echo(f"out:          {out_dir}")
    return release_manifest


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build the release provenance bundle (SBOM, changelog, rollback, manifest).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--package-dir",
        type=Path,
        default=PACKAGE_DIR,
        help=f"the packaged app output (manifest.json, dmg, CHECKSUMS.sha256); default: {PACKAGE_DIR}",
    )
    parser.add_argument(
        "--out-dir", type=Path, default=OUT_DIR, help=f"where to write provenance; default: {OUT_DIR}"
    )
    parser.add_argument(
        "--tag",
        default=None,
        help="the release tag (e.g. v1.2.0); default: the artifact's git describe",
    )
    parser.add_argument(
        "--attestation-status",
        choices=ATTESTATION_STATUSES,
        default=ATTESTATION_UNKNOWN,
        help=(
            "whether GitHub artifact attestation is entitled for this repo, as found "
            "by the release workflow's preflight; recorded in the release manifest. "
            f"default: {ATTESTATION_UNKNOWN} (a local dry run that never reaches the API)"
        ),
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(argv)

    try:
        generate(
            args.repo_root,
            package_dir=args.package_dir,
            out_dir=args.out_dir,
            tag=args.tag,
            attestation_status=args.attestation_status,
        )
    except ReleaseError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
