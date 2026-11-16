#!/usr/bin/env python3
"""Resolve the one server executable Cargo just reported, not a guessed target path."""
import json
from pathlib import Path
import sys


def _one(lines, harness):
    """The one mc-spine binary Cargo reported, for the build or the test harness.

    `cargo build` reports `test: false`; `cargo test --no-run` reports the harness
    with `test: true`. They are different binaries, so the caller says which it
    means instead of accepting whichever appears.
    """
    paths = set()
    for line in lines:
        event = json.loads(line)
        target = event.get("target", {})
        profile = event.get("profile", {})
        if (event.get("reason") == "compiler-artifact" and target.get("name") == "mc-spine"
                and target.get("kind") == ["bin"] and bool(profile.get("test")) is harness
                and event.get("executable")):
            paths.add(Path(event["executable"]).resolve())
    kind = "test harness" if harness else "build"
    if len(paths) != 1:
        raise ValueError(f"expected one mc-spine {kind} executable, received {len(paths)}")
    path = paths.pop()
    if not path.is_file():
        raise ValueError(f"Cargo's executable does not exist: {path}")
    return path


def executable(lines):
    return _one(lines, harness=False)


def test_executable(lines):
    return _one(lines, harness=True)


if __name__ == "__main__":
    print(executable(sys.stdin))
