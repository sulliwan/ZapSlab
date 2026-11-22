#!/usr/bin/env python3
"""Bump the app version in the root Cargo.toml.

Usage: bump_version.py major|minor|revision

The semantic version is display-only: the palette shows it, while deploy
detection uses the wasm content hash (see scripts/build_web.sh), so the version
does not need to change for every build. `revision` maps to the semver patch
component. Cargo rewrites Cargo.lock on the next build.
"""

import re
import sys
from pathlib import Path

CARGO_TOML = Path(__file__).resolve().parent.parent / "Cargo.toml"
PARTS = ("major", "minor", "revision")


def main() -> int:
    part = sys.argv[1] if len(sys.argv) == 2 else None
    if part not in PARTS:
        print("usage: make bump_version major|minor|revision", file=sys.stderr)
        return 1

    text = CARGO_TOML.read_text(encoding="utf-8")
    # Only the package version starts a line with `version =`; dependency
    # versions are written inline as `{ version = "..." }`.
    match = re.search(r'^version = "(\d+)\.(\d+)\.(\d+)"', text, flags=re.MULTILINE)
    if not match:
        print("error: could not find the package version in Cargo.toml", file=sys.stderr)
        return 1

    major, minor, revision = (int(value) for value in match.groups())
    if part == "major":
        major, minor, revision = major + 1, 0, 0
    elif part == "minor":
        minor, revision = minor + 1, 0
    else:
        revision += 1

    old = match.group(0).split('"')[1]
    new = f"{major}.{minor}.{revision}"
    text = text[: match.start()] + f'version = "{new}"' + text[match.end() :]
    CARGO_TOML.write_text(text, encoding="utf-8")
    print(f"Cargo.toml: {old} -> {new}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
