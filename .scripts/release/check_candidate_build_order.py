#!/usr/bin/env python3
"""Resolve the highest published Sparkle build without mutating a feed."""
from __future__ import annotations

import argparse
import os
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from collections.abc import Callable, Sequence

TEST_FEED = "https://github.com/relux-works/tgfs/releases/download/updates-test-v1/test.xml"
STABLE_FEED = "https://relux-works.github.io/tgfs/updates/stable/v1/stable.xml"
Fetch = Callable[[str], bytes | None]


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
    result: list[int] = []
    for element in root.iter():
        for name, value in element.attrib.items():
            if name.endswith("}version") or name == "sparkle:version":
                if not value.isdigit():
                    raise ValueError(f"non-numeric sparkle version {value!r}")
                result.append(int(value))
    return result


def highest(mode: str, loader: Fetch = fetch) -> tuple[int, list[str]]:
    urls = [TEST_FEED] if mode == "test" else [TEST_FEED, STABLE_FEED]
    observed: list[int] = []
    present: list[str] = []
    for url in urls:
        payload = loader(url)
        if payload is None:
            continue
        observed.extend(builds(payload))
        present.append(url)
    return max(observed, default=0), present


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("test", "stable-candidate"), required=True)
    args = parser.parse_args(argv)
    try:
        value, feeds = highest(args.mode)
    except (OSError, ValueError, ET.ParseError) as error:
        print(f"CANDIDATE BUILD ORDER FAILED: {error}", file=sys.stderr)
        return 1
    print(f"highest applicable published build: {value}; feeds present: {len(feeds)}")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"minimum_build={value}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
