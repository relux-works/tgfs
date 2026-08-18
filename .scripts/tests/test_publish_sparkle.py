#!/usr/bin/env python3
from __future__ import annotations

import base64
import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
import xml.etree.ElementTree as ET
from hashlib import sha256
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / ".scripts" / "release" / "publish_sparkle.py"


def load_module():
    spec = importlib.util.spec_from_file_location("publish_sparkle", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


pub = load_module()
COMMIT = "a" * 40
SIGNATURE = base64.b64encode(b"s" * 64).decode()


def write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")


def stage_candidate(root: Path, *, mode: str = "test", build: int = 100, version: str = "1.2.3") -> Path:
    root.mkdir(parents=True)
    channel = "stable" if mode == "stable-candidate" else "test"
    dmg_name = f"GramDrive-{version}-{build}.dmg"
    (root / dmg_name).write_bytes(f"dmg-{build}".encode())
    digest = sha256((root / dmg_name).read_bytes()).hexdigest()
    source = {"repository": pub.REPOSITORY, "commit": COMMIT, "ref": "refs/heads/main"}
    workflow = {
        "ref": f"{pub.REPOSITORY}/.github/workflows/candidate-build.yml@refs/heads/main",
        "run_id": "321",
        "run_attempt": "1",
    }
    manifest = {
        "schema": 1,
        "source": source,
        "workflow": workflow,
        "mode": mode,
        "channel": channel,
        "version": {"short": version, "build": str(build)},
        "identity": {"team_id": pub.TEAM_ID},
        "dmg": {"name": dmg_name, "sha256": digest, "bytes": (root / dmg_name).stat().st_size},
        "attestation": {"file": "candidate-attestation.json"},
        "publication": {"owned": False, "downstream_task": "TASK-260810-y3zcg8"},
    }
    write_json(root / "candidate-manifest.json", manifest)
    write_json(root / "verification.json", {"schema": 1, "result": "passed", "gates": {"checksums": "passed", "signatures": "passed"}})
    feed_url = (
        f"https://github.com/{pub.REPOSITORY}/releases/download/updates-test-v1/test.xml"
        if channel == "test"
        else "https://relux-works.github.io/tgfs/updates/stable/v1/stable.xml"
    )
    write_json(root / "app-manifest.json", {"sparkle": {"channel": channel, "feed_url": feed_url}})
    write_json(root / "candidate-provenance.json", {"source": source, "workflow": workflow, "candidate": {"mode": mode, "channel": channel}})
    write_json(root / "candidate-attestation.json", {"bundle": "fixture"})
    attestation_digest = pub.sha256_file(root / "candidate-attestation.json")
    write_json(root / "finalization.json", {"schema": 1, "status": "verified-and-attested", "privacy_scrub": "passed", "attestation": {"sha256": attestation_digest}})
    for name in ("SUBJECTS.sha256", "app-checksums.sha256", "core-checksums.sha256", "core-manifest.json", "tdlib-checksums.sha256", "tdlib-manifest.json"):
        (root / name).write_text("fixture\n", encoding="utf-8")
    refresh_checksums(root)
    return root


def refresh_checksums(root: Path) -> None:
    entries = []
    for path in sorted(root.iterdir()):
        if path.is_file() and path.name != "CANDIDATE-CHECKSUMS.sha256":
            entries.append(f"{pub.sha256_file(path)}  {path.name}\n")
    (root / "CANDIDATE-CHECKSUMS.sha256").write_text("".join(entries), encoding="utf-8")


def sig(path: Path, length: int, *, notes: bool = False) -> None:
    prefix = "sparkle:length" if notes else "length"
    warning = "<!-- Updated notes.md by adding warning for making further modifications. -->\n" if notes else ""
    path.write_text(warning + f'sparkle:edSignature="{SIGNATURE}" {prefix}="{length}"\n', encoding="utf-8")


def append_signature(path: Path, signature: bytes) -> None:
    content = path.read_bytes()
    encoded = base64.b64encode(signature).decode()
    path.write_bytes(content + f"<!-- sparkle-signatures:\nedSignature: {encoded}\nlength: {len(content)}\n-->\n".encode())


def render(root: Path, output: Path, *, channel: str, generation: int = 1, prior: Path | None = None) -> None:
    manifest = pub.verify_candidate(root)
    notes = output.parent / "notes.md"
    notes.write_text("notes\n", encoding="utf-8")
    dmg_sig = output.parent / "dmg.sig"
    notes_sig = output.parent / "notes.sig"
    sig(dmg_sig, manifest["dmg"]["bytes"])
    sig(notes_sig, notes.stat().st_size, notes=True)
    pub.render_feed(
        manifest,
        channel=channel,
        generation=generation,
        repository=pub.REPOSITORY,
        dmg_signature_path=dmg_sig,
        notes_signature_path=notes_sig,
        notes_name=f"GramDrive-{manifest['version']['short']}-{manifest['version']['build']}.md",
        publication_date="Tue, 18 Aug 2026 12:00:00 +0000",
        prior_feed=prior,
        output=output,
    )


class CandidateIntakeTests(unittest.TestCase):
    def test_exact_verified_candidate_is_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = pub.verify_candidate(stage_candidate(Path(tmp) / "candidate"), run_id="321", commit=COMMIT, mode="test")
            self.assertEqual(manifest["dmg"]["name"], "GramDrive-1.2.3-100.dmg")

    def test_tampered_dmg_is_rejected_before_publication(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = stage_candidate(Path(tmp) / "candidate")
            (root / "GramDrive-1.2.3-100.dmg").write_bytes(b"changed")
            with self.assertRaisesRegex(pub.PublicationError, "checksum mismatch"):
                pub.verify_candidate(root)

    def test_wrong_workflow_or_publication_owner_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = stage_candidate(Path(tmp) / "candidate")
            manifest = json.loads((root / "candidate-manifest.json").read_text())
            manifest["publication"]["downstream_task"] = "someone-else"
            write_json(root / "candidate-manifest.json", manifest)
            refresh_checksums(root)
            with self.assertRaisesRegex(pub.PublicationError, "delegate publication"):
                pub.verify_candidate(root)

    def test_stable_requires_exact_tag_version_and_stable_candidate_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = stage_candidate(Path(tmp) / "candidate", mode="stable-candidate")
            self.assertEqual(pub.verify_candidate(root, mode="stable-candidate", version="1.2.3")["channel"], "stable")
            with self.assertRaisesRegex(pub.PublicationError, "version"):
                pub.verify_candidate(root, version="1.2.4")


class ArchiveTests(unittest.TestCase):
    def test_archive_is_deterministic_and_round_trips_exact_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            root = stage_candidate(base / "candidate")
            first, second = base / "first.tar.gz", base / "second.tar.gz"
            pub.pack_tree(root, first)
            pub.pack_tree(root, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            pub.unpack_tree(first, base / "restored", flat=True)
            self.assertEqual(
                {p.name: p.read_bytes() for p in root.iterdir()},
                {p.name: p.read_bytes() for p in (base / "restored").iterdir()},
            )

    def test_archive_rejects_traversal_and_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            archive = base / "bad.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                info = tarfile.TarInfo("../escape")
                info.size = 1
                tar.addfile(info, io.BytesIO(b"x"))
            with self.assertRaisesRegex(pub.PublicationError, "unsafe archive"):
                pub.unpack_tree(archive, base / "out")


class FeedTests(unittest.TestCase):
    def test_ed25519_verifier_matches_rfc8032_vector(self):
        public = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        signature = bytes.fromhex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
            "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        )
        self.assertTrue(pub.verify_ed25519(public, b"", signature))
        self.assertFalse(pub.verify_ed25519(public, b"changed", signature))

    def test_test_feed_accepts_tested_stable_candidate_but_uses_only_release_urls(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            root = stage_candidate(base / "candidate", mode="stable-candidate")
            feed = base / "test.xml"
            render(root, feed, channel="test")
            pub.validate_feed(feed, channel="test", generation=1, repository=pub.REPOSITORY)
            text = feed.read_text()
            self.assertIn("/releases/download/updates-test-v1/GramDrive-1.2.3-100.dmg", text)
            self.assertNotIn("github.io", text)

    def test_stable_feed_uses_only_tagged_release_and_versioned_pages_notes(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            root = stage_candidate(base / "candidate", mode="stable-candidate")
            feed = base / "stable.xml"
            render(root, feed, channel="stable")
            pub.validate_feed(feed, channel="stable", generation=1, repository=pub.REPOSITORY)
            text = feed.read_text()
            self.assertIn("/releases/download/v1.2.3/GramDrive-1.2.3-100.dmg", text)
            self.assertIn("/updates/stable/v1/notes/", text)
            self.assertNotIn("updates-test-v1", text)

    def test_feed_refuses_non_monotonic_build_and_cross_channel_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            newer = stage_candidate(base / "newer", build=101)
            prior = base / "prior.xml"
            render(newer, prior, channel="test")
            older = stage_candidate(base / "older", build=100)
            with self.assertRaisesRegex(pub.PublicationError, "not newer"):
                render(older, base / "bad.xml", channel="test", prior=prior)
            prior.write_text(prior.read_text().replace("updates-test-v1", "v1.2.3"))
            newest = stage_candidate(base / "newest", build=102)
            with self.assertRaisesRegex(pub.PublicationError, "escapes the test endpoint"):
                render(newest, base / "bad2.xml", channel="test", prior=prior)

    def test_test_feed_retains_current_and_previous_ten(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            prior = None
            for build in range(100, 112):
                root = stage_candidate(base / f"candidate-{build}", build=build)
                output = base / f"feed-{build}.xml"
                render(root, output, channel="test", prior=prior)
                prior = output
            items = ET.parse(prior).getroot().find("channel").findall("item")
            self.assertEqual([pub._item_build(item) for item in items], list(range(111, 100, -1)))

    def test_complete_site_archive_preserves_frozen_old_generation_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            site = base / "site"
            old = site / "updates/stable/v1/stable.xml"
            old.parent.mkdir(parents=True)
            old.write_bytes(b"old-key-signed-bridge-feed")
            new = site / "updates/stable/v2/stable.xml"
            new.parent.mkdir(parents=True)
            new.write_bytes(b"new-key-feed")
            archive = base / "site.tar.gz"
            pub.pack_tree(site, archive)
            pub.unpack_tree(archive, base / "restored")
            self.assertEqual((base / "restored/updates/stable/v1/stable.xml").read_bytes(), old.read_bytes())

    def _authenticated_site(self, base: Path):
        site = base / "site"
        keys = {1: base64.b64encode(b"1" * 32).decode(), 2: base64.b64encode(b"2" * 32).decode()}
        accepted: dict[tuple[bytes, bytes], bytes] = {}
        for generation, marker in ((1, b"a"), (2, b"b")):
            candidate = stage_candidate(base / f"candidate-{generation}", mode="stable-candidate", build=99 + generation)
            feed = site / f"updates/stable/v{generation}/stable.xml"
            feed.parent.mkdir(parents=True)
            render(candidate, feed, channel="stable", generation=generation)
            notes_name = f"GramDrive-1.2.3-{99 + generation}.md"
            notes = site / f"updates/stable/v{generation}/notes/{notes_name}"
            notes.parent.mkdir()
            notes.write_bytes((feed.parent / "notes.md").read_bytes())
            (feed.parent / "notes.md").unlink()
            content = feed.read_bytes()
            feed_signature = marker * 64
            append_signature(feed, feed_signature)
            key = base64.b64decode(keys[generation])
            accepted[(key, content)] = feed_signature
            accepted[(key, notes.read_bytes())] = b"s" * 64
        archive = base / "stable-pages-site.tar.gz"
        pub.pack_tree(site, archive)
        manifest = base / "stable-pages-site-manifest.json"
        pub.write_site_manifest(site, archive, keys, 2, manifest)
        manifest_signature = base / "stable-pages-site-manifest.signature.txt"
        sig_bytes = b"m" * 64
        manifest_signature.write_text(
            f'sparkle:edSignature="{base64.b64encode(sig_bytes).decode()}" length="{manifest.stat().st_size}"\n'
        )
        accepted[(base64.b64decode(keys[2]), manifest.read_bytes())] = sig_bytes
        return site, archive, manifest, manifest_signature, keys, accepted

    def test_authenticated_site_verifies_every_generation_and_exact_inventory(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            site, archive, manifest, signature_path, _keys, accepted = self._authenticated_site(base)
            verifier = lambda key, data, signature: accepted.get((key, data)) == signature
            pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)

            (site / "extra.txt").write_text("extra")
            with self.assertRaisesRegex(pub.PublicationError, "inventory changed"):
                pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)
            (site / "extra.txt").unlink()

            notes = site / "updates/stable/v1/notes/GramDrive-1.2.3-100.md"
            notes.write_text("changed")
            with self.assertRaisesRegex(pub.PublicationError, "inventory changed"):
                pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)

    def test_site_rejects_replaced_archive_modified_feed_and_wrong_generation_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            site, archive, manifest, signature_path, keys, accepted = self._authenticated_site(base)
            verifier = lambda key, data, signature: accepted.get((key, data)) == signature

            archive.write_bytes(archive.read_bytes() + b"changed")
            with self.assertRaisesRegex(pub.PublicationError, "archive digest changed"):
                pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)

            pub.pack_tree(site, archive)
            feed = site / "updates/stable/v1/stable.xml"
            original_feed = feed.read_bytes()
            feed.write_bytes(original_feed.replace(b"GramDrive stable", b"GramDrive broken", 1))
            pub.pack_tree(site, archive)
            pub.write_site_manifest(site, archive, keys, 2, manifest)
            sig(signature_path, manifest.stat().st_size)
            accepted[(base64.b64decode(keys[2]), manifest.read_bytes())] = b"s" * 64
            with self.assertRaisesRegex(pub.PublicationError, "feed EdDSA signature is invalid"):
                pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)

            feed.write_bytes(original_feed)
            pub.pack_tree(site, archive)
            wrong_key = base64.b64encode(b"3" * 32).decode()
            keys[1] = wrong_key
            pub.write_site_manifest(site, archive, keys, 2, manifest)
            sig(signature_path, manifest.stat().st_size)
            accepted[(base64.b64decode(keys[2]), manifest.read_bytes())] = b"s" * 64
            with self.assertRaisesRegex(pub.PublicationError, "feed EdDSA signature is invalid"):
                pub.verify_site(site, archive, manifest, signature_path, verifier=verifier)

    def test_stable_promotion_requires_candidate_in_signed_test_offer(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            candidate = stage_candidate(base / "candidate", mode="stable-candidate")
            feed = base / "test.xml"
            render(candidate, feed, channel="test")
            notes = base / "GramDrive-1.2.3-100.md"
            notes.write_bytes((base / "notes.md").read_bytes())
            content = feed.read_bytes()
            feed.write_bytes(
                content
                + f"<!-- sparkle-signatures:\nedSignature: {SIGNATURE}\nlength: {len(content)}\n-->\n".encode()
            )
            pub.verify_test_offer(candidate, feed, notes, verifier=lambda _key, _data, _signature: True)
            tampered = feed.read_bytes().replace(b"updates-test-v1", b"updates-test-v2")
            content = tampered[:tampered.rfind(b"<!-- sparkle-signatures:")]
            feed.write_bytes(
                content
                + f"<!-- sparkle-signatures:\nedSignature: {SIGNATURE}\nlength: {len(content)}\n-->\n".encode()
            )
            with self.assertRaisesRegex(pub.PublicationError, "escapes the test endpoint"):
                pub.verify_test_offer(candidate, feed, notes, verifier=lambda _key, _data, _signature: True)

    def test_rotation_bridges_same_candidate_then_freezes_old_generation(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            bridge_candidate = stage_candidate(base / "bridge", mode="stable-candidate", build=200)
            old_feed = base / "v1.xml"
            new_feed = base / "v2.xml"
            render(bridge_candidate, old_feed, channel="stable", generation=1)
            render(bridge_candidate, new_feed, channel="stable", generation=2)
            old_item = ET.parse(old_feed).getroot().find("channel/item")
            new_item = ET.parse(new_feed).getroot().find("channel/item")
            self.assertEqual(pub._item_build(old_item), pub._item_build(new_item))
            self.assertEqual(old_item.find("enclosure").get("url"), new_item.find("enclosure").get("url"))
            self.assertIn("/stable/v1/notes/", old_item.find(f"{{{pub.SPARKLE_NS}}}releaseNotesLink").text)
            self.assertIn("/stable/v2/notes/", new_item.find(f"{{{pub.SPARKLE_NS}}}releaseNotesLink").text)
            frozen_old = old_feed.read_bytes()

            next_candidate = stage_candidate(base / "next", mode="stable-candidate", build=201)
            render(next_candidate, base / "v2-next.xml", channel="stable", generation=2, prior=new_feed)
            self.assertEqual(old_feed.read_bytes(), frozen_old)


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.candidate = (REPO_ROOT / ".github/workflows/candidate-build.yml").read_text()
        cls.stable = (REPO_ROOT / ".github/workflows/release.yml").read_text()

    def test_test_publisher_has_only_contents_write_test_key_and_no_pages(self):
        job = self.candidate[self.candidate.index("  publish-test:"):]
        self.assertIn("contents: write", job)
        self.assertIn("SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64", job)
        for forbidden in ("pages: write", "SPARKLE_STABLE", "MACOS_CERT_P12", "APPSTORE_PRIVATE_KEY", "notarytool", "codesign"):
            self.assertNotIn(forbidden, job)

    def test_test_feed_is_last_mutation_and_has_explicit_restore(self):
        job = self.candidate[self.candidate.index("  publish-test:"):]
        self.assertLess(job.index("upload_immutable \".temp/publication/out/$PUBLICATION_PACKAGE\""), job.index("test.xml --clobber"))
        self.assertIn("restore_prior", job)
        self.assertIn("cmp .temp/publication/out/test.xml", job)

    def test_stable_promotion_never_builds_or_resigns_apple_code(self):
        promote = self.stable[self.stable.index("  promote:"):self.stable.index("  deploy-pages:")]
        for forbidden in ("MACOS_CERT_P12", "APPSTORE_PRIVATE_KEY", "notarytool", "codesign", "make package", "swift build", "cargo build"):
            self.assertNotIn(forbidden, promote)
        self.assertIn("steps.candidate.outputs.dmg_name", promote)
        self.assertIn("verify-test-offer", promote)
        self.assertIn("SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64", promote)

    def test_approved_rotation_signs_old_bridge_and_new_feed_with_distinct_keys(self):
        promote = self.stable[self.stable.index("  promote:"):self.stable.index("  recover-pages:")]
        self.assertIn("- rotate-key", self.stable)
        self.assertIn("SPARKLE_STABLE_PREVIOUS_EDDSA_PRIVATE_KEY_B64", promote)
        self.assertIn("old generation/public key does not match the authenticated prior site", promote)
        self.assertIn('"${{ steps.source.outputs.previous_generation }}"', promote)
        self.assertIn("rotation refuses to create a bridge", promote)
        self.assertIn('"${{ steps.source.outputs.generation }}"', promote)

    def test_candidate_binds_rotated_stable_clients_but_test_clients_are_v1_disposable(self):
        self.assertIn("stable_feed_generation", self.candidate)
        self.assertIn("--update-feed-generation", self.candidate)
        publisher = self.candidate[self.candidate.index("  publish-test:"):]
        self.assertIn("updates-test-v1", publisher)
        self.assertNotIn("SPARKLE_TEST_V2", publisher)

    def test_pages_capability_is_isolated_from_stable_key_and_contents_write(self):
        deploy = self.stable[self.stable.index("  deploy-pages:"):]
        self.assertIn("pages: write", deploy)
        self.assertIn("id-token: write", deploy)
        self.assertNotIn("contents: write", deploy)
        self.assertNotIn("SPARKLE_STABLE", deploy)
        promote = self.stable[self.stable.index("  promote:"):self.stable.index("  deploy-pages:")]
        self.assertNotIn("pages: write", promote)
        self.assertIn("environment: release", promote)

    def test_old_complete_site_has_approval_gated_keyless_recovery(self):
        recovery = self.stable[self.stable.index("  recover-pages:"):self.stable.index("  deploy-pages:")]
        self.assertIn("environment: release", recovery)
        self.assertIn("stable-pages-site.tar.gz", recovery)
        self.assertIn("unpack-tree", recovery)
        self.assertIn("stable-pages-site.attestation.json", recovery)
        self.assertIn("gh attestation verify", recovery)
        self.assertIn("verify-site", recovery)
        self.assertNotIn("contents: write", recovery)
        self.assertNotIn("SPARKLE_STABLE", recovery)
        deploy = self.stable[self.stable.index("  deploy-pages:"):]
        self.assertIn("needs: [promote, recover-pages]", deploy)


if __name__ == "__main__":
    unittest.main()
