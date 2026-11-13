#!/usr/bin/env python3
"""Verify native floor/anchor predicates using the shared name/property translator."""
import argparse
import json
from pathlib import Path
from block_predicate_oracle import encode
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
ASSET = ROOT / "assets/mineshaft_support_1211.bin"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--output", type=Path, help="explicit destination; otherwise check without writing")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "support.tsv"
        run_probe(work, classpath, ROOT / "tools/structure-oracle/MineshaftSupportOracle.java",
                  "dev.mcspine.tools.MineshaftSupportOracle", output,
                  extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"])
        data, _, _ = encode(output.read_text(), json.loads((ROOT / "assets/blocks_1211.json").read_text()))
    if args.output:
        args.output.write_bytes(data)
    elif data != ASSET.read_bytes():
        raise SystemExit("mineshaft supports differ (no files changed)")
    print(f"mineshaft support oracle: {len(data)} states " + ("written" if args.output else "match"))


if __name__ == "__main__":
    main()
