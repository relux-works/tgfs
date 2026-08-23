#!/usr/bin/env python3

from __future__ import annotations

import base64
import importlib.util
import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SMOKE_PATH = REPO_ROOT / ".scripts" / "smoke" / "run_sparkle_manual_update_smoke.py"
PACKAGER_PATH = REPO_ROOT / ".scripts" / "apple-app" / "build_app_bundle.py"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


smoke = load_module("sparkle_manual_update_smoke", SMOKE_PATH)
packager = load_module("sparkle_smoke_packager", PACKAGER_PATH)


class SparkleManualUpdateSmokeTests(unittest.TestCase):
    def test_fixture_key_is_valid_and_distinct_from_every_shipping_anchor(self):
        self.assertEqual(len(smoke.FIXTURE_PRIVATE_ED_KEY_BYTES), 96)
        self.assertEqual(len(base64.b64decode(smoke.FIXTURE_PUBLIC_ED_KEY, validate=True)), 32)
        production_keys = {
            configuration["public_key"] for configuration in packager.UPDATE_CHANNELS.values()
        }
        self.assertNotIn(smoke.FIXTURE_PUBLIC_ED_KEY, production_keys)

    def test_accessory_host_keeps_signed_feed_and_extraction_verification_enabled(self):
        plist = smoke.host_info_plist("http://127.0.0.1:12345/appcast.xml")
        self.assertIs(plist["LSUIElement"], True)
        self.assertIs(plist["SURequireSignedFeed"], True)
        self.assertIs(plist["SUVerifyUpdateBeforeExtraction"], True)
        self.assertEqual(plist["SUPublicEDKey"], smoke.FIXTURE_PUBLIC_ED_KEY)

    def test_result_contract_requires_zero_window_visible_key_capable_presentation(self):
        payload = {
            "initialQualifyingWindowCount": 0,
            "windowTitle": "A new version is available!",
            "windowIsVisible": True,
            "windowCanBecomeKey": True,
            "activationPolicy": "regular",
            "applicationIsActive": True,
            "hostBuild": "1",
            "offeredBuild": "2",
        }
        result = smoke.parse_result(
            smoke.RESULT_PREFIX + __import__("json").dumps(payload, sort_keys=True)
        )
        self.assertEqual(result, payload)

    def test_privacy_safe_output_redacts_repository_and_home_paths(self):
        rendered = smoke.privacy_safe(str(smoke.REPO_ROOT / ".temp" / "fixture"))
        self.assertEqual(rendered, "<repo>/.temp/fixture")


if __name__ == "__main__":
    unittest.main()
