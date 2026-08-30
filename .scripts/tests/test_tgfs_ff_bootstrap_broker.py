from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import socket
import struct
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "tgfs_ff_bootstrap_broker.py"
SPEC = importlib.util.spec_from_file_location("tgfs_ff_bootstrap_broker", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
broker = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = broker
SPEC.loader.exec_module(broker)


class FixedGitHubTests(unittest.TestCase):
    def test_link_parser_rejects_duplicate_and_malformed_cursors(self) -> None:
        self.assertEqual(
            broker.parse_links('<https://api.github.com/next>; rel="next"'),
            {"next": "https://api.github.com/next"},
        )
        with self.assertRaises(broker.Refused):
            broker.parse_links(
                '<https://api.github.com/a>; rel="next", '
                '<https://api.github.com/b>; rel="next"'
            )
        with self.assertRaises(broker.Refused):
            broker.parse_links("garbage")

    def test_non_fixed_endpoint_is_rejected_before_urlopen(self) -> None:
        client = broker.FixedGitHub(bytearray(b"ghp_test"))
        with mock.patch("urllib.request.urlopen") as urlopen:
            with self.assertRaises(broker.Refused):
                client.request("https://example.invalid/")
        urlopen.assert_not_called()
        client.close()

    def test_page_cursor_loop_and_malformed_json_fail_closed(self) -> None:
        client = broker.FixedGitHub(bytearray(b"ghp_test"))
        response = mock.MagicMock()
        response.__enter__.return_value = response
        response.read.return_value = b"[]"
        response.headers.items.return_value = [
            ("Link", '<https://api.github.com/loop>; rel="next"')
        ]
        with mock.patch("urllib.request.urlopen", return_value=response):
            with self.assertRaises(broker.Refused):
                client.pages("/loop")
        response.read.return_value = b"not-json"
        response.headers.items.return_value = []
        with mock.patch("urllib.request.urlopen", return_value=response):
            with self.assertRaises(broker.Refused):
                client.pages("/malformed")
        client.close()


class AuditTests(unittest.TestCase):
    def test_durable_write_is_create_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "intent.json"
            broker.durable_write(path, {"old_id": "a" * 40})
            self.assertEqual(json.loads(path.read_bytes())["old_id"], "a" * 40)
            with self.assertRaises(FileExistsError):
                broker.durable_write(path, {"old_id": "b" * 40})

    def test_frame_rejects_zero_and_truncated_payloads(self) -> None:
        left, right = socket.socketpair()
        try:
            right.sendall(struct.pack(">I", 0))
            with self.assertRaises(broker.Refused):
                broker.read_frame(left)
        finally:
            left.close()
            right.close()


class CapabilityTests(unittest.TestCase):
    def test_broker_has_no_caller_selected_repository_or_endpoint(self) -> None:
        source = SCRIPT.read_text()
        self.assertIn('REPOSITORY = "relux-works/tgfs"', source)
        self.assertIn('MAIN = "refs/heads/main"', source)
        self.assertNotIn("argparse", source)
        self.assertNotIn('"curl"', source)
        self.assertNotIn("force-with-lease", source)
        self.assertEqual(source.count('"/opt/homebrew/bin/gh"'), 1)
        self.assertIn(
            '["/opt/homebrew/bin/gh", "auth", "token", "--hostname", "github.com"]',
            source,
        )


if __name__ == "__main__":
    unittest.main()
