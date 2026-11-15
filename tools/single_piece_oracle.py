#!/usr/bin/env python3
"""Generate or verify single-piece procedural box structure vectors against 1.21.1."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests/data/single_piece_structures_1211.tsv"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output", type=Path, help="write TSV to this path; otherwise verify against fixture")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, classpath, _):
        out = work / "single_piece.tsv"
        run_probe(
            work,
            classpath,
            ROOT / "tools/structure-oracle/SinglePieceOracle.java",
            "dev.mcspine.tools.SinglePieceOracle",
            out,
            extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"],
        )
        text = out.read_text()
    if a.output:
        a.output.write_text(text)
        print(f"single_piece: wrote {len(text.splitlines())} vectors to {a.output}")
    else:
        if text != FIXTURE.read_text():
            raise SystemExit("single_piece vectors differ (no files changed)")
        print(f"single_piece: {len(text.splitlines())} vectors match")


if __name__ == "__main__":
    main()
