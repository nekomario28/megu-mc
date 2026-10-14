#!/usr/bin/env python3
"""Resolve the one server executable Cargo just reported, not a guessed target path."""
import json
from pathlib import Path
import sys


def executable(lines):
    paths = set()
    for line in lines:
        event = json.loads(line)
        target = event.get("target", {})
        if (event.get("reason") == "compiler-artifact" and target.get("name") == "mc-spine"
                and target.get("kind") == ["bin"] and not event.get("profile", {}).get("test")
                and event.get("executable")):
            paths.add(Path(event["executable"]).resolve())
    if len(paths) != 1:
        raise ValueError(f"expected one mc-spine build executable, received {len(paths)}")
    path = paths.pop()
    if not path.is_file():
        raise ValueError(f"Cargo's executable does not exist: {path}")
    return path


if __name__ == "__main__":
    print(executable(sys.stdin))
