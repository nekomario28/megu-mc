#!/usr/bin/env python3
"""Generate or verify the fortress/monument piece-tree and placement vectors against 1.21.1."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests/data/fortress_monument_1211.tsv"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output", type=Path,
                   help="write TSV to this path; otherwise verify against the checked-in fixture")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, classpath, _):
        out = work / "fortress_monument.tsv"
        run_probe(
            work,
            classpath,
            ROOT / "tools/structure-oracle/FortressMonumentOracle.java",
            "dev.mcspine.tools.FortressMonumentOracle",
            out,
            extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"],
        )
        text = out.read_text()
    if a.output:
        a.output.write_text(text)
        print(f"fortress_monument: wrote {len(text.splitlines())} vectors to {a.output}")
    else:
        if text != FIXTURE.read_text():
            raise SystemExit("fortress_monument vectors differ (no files changed)")
        print(f"fortress_monument: {len(text.splitlines())} vectors match")


if __name__ == "__main__":
    main()
