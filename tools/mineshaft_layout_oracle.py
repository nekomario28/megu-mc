#!/usr/bin/env python3
"""Check NORMAL mineshaft layout vectors against the pinned native body (no admission)."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / "tests/data/mineshaft_layout_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", type=Path, default=VECTORS)
    mode.add_argument("--output", type=Path)
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        out = work / "layout.tsv"
        run_probe(work, cp, ROOT / "tools/structure-oracle/MineshaftLayoutOracle.java",
                  "dev.mcspine.tools.MineshaftLayoutOracle", out,
                  extra_sources=(ROOT / "tools/structure-oracle/NativeHarness.java",))
        actual = out.read_bytes()
    if not actual.startswith(b"C\t") or b"\nP\t" not in actual:
        raise SystemExit("mineshaft layout oracle: empty/incomplete native vectors")
    if args.output:
        args.output.write_bytes(actual)
    elif actual != args.check.read_bytes():
        raise SystemExit("mineshaft layout oracle: vectors differ (no files changed)")
    print("mineshaft layout oracle: " + ("written" if args.output else "match"))


if __name__ == "__main__":
    main()
