#!/usr/bin/env python3
"""Compare generated leaf-tick identities and per-chunk order to a native trace."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("native_log", type=Path)
    parser.add_argument("port_ticks", type=Path)
    args = parser.parse_args()
    pattern = re.compile(r"TICKTRACE proto-add (-?\d+) (-?\d+) (-?\d+) type=Block\{(minecraft:[^}]*_leaves)\}")
    native, port = defaultdict(list), defaultdict(list)
    with args.native_log.open() as source:
        for line in source:
            match = pattern.search(line)
            if match:
                x, y, z = map(int, match.group(1, 2, 3))
                native[(x >> 4, z >> 4)].append((x, y, z, match[4]))
    with args.port_ticks.open() as source:
        for line in source:
            x, y, z, name = line.split()
            x, y, z = int(x), int(y), int(z)
            if -64 <= x >> 4 <= -62 and 32 <= z >> 4 <= 34:
                port[(x >> 4, z >> 4)].append((x, y, z, name))
    keys = {(x, z) for x in range(-64, -61) for z in range(32, 35)}
    expected = {v for key in keys for v in native[key]}
    actual = {v for key in keys for v in port[key]}
    report = {"native_events": sum(len(native[k]) for k in keys),
              "port_events": sum(len(port[k]) for k in keys),
              "native_unique": len(expected), "port_unique": len(actual),
              "native_only": len(expected - actual), "port_only": len(actual - expected),
              "chunks_with_different_order": [list(k) for k in sorted(keys) if native[k] != port[k]],
              "sample_native_only": sorted(expected - actual)[:10],
              "sample_port_only": sorted(actual - expected)[:10]}
    print(json.dumps(report, indent=2))
    if not expected or report["native_events"] != len(expected) or report["port_events"] != len(actual):
        raise SystemExit("empty or duplicate tick input")
    if expected != actual or report["chunks_with_different_order"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
