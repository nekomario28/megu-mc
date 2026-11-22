#!/usr/bin/env python3
"""Native 1.21.1 Beardifier density and StructurePlacement admission vectors.

Runs `tools/structure-oracle/BeardifierPipelineOracle.java` against the pinned server
jar and either writes the TSV fixture or checks the committed one for drift.
"""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests/data/beardifier_pipeline_1211.tsv"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output", type=Path,
                   help="explicit destination; otherwise compare with the committed fixture")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, classpath, _):
        out = work / "beardifier_pipeline.tsv"
        run_probe(work, classpath,
                  ROOT / "tools/structure-oracle/BeardifierPipelineOracle.java",
                  "dev.megu.tools.BeardifierPipelineOracle", str(out),
                  extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"])
        rows = out.read_text().splitlines()
    kinds = {}
    for row in rows:
        kinds[row.split("\t")[0]] = kinds.get(row.split("\t")[0], 0) + 1
    scenes = sum(1 for r in rows if r.startswith("SCENE\t"))
    admits = kinds.get("ADMIT", 0)
    samples = kinds.get("SAMPLE", 0)
    if scenes == 0 or admits == 0 or samples == 0:
        raise SystemExit("beardifier pipeline oracle produced no vectors")
    if a.output:
        a.output.parent.mkdir(parents=True, exist_ok=True)
        a.output.write_text("\n".join(rows) + "\n")
    else:
        want = FIXTURE.read_text().splitlines()
        if rows != want:
            for i, (got, exp) in enumerate(zip(rows, want)):
                if got != exp:
                    raise SystemExit(f"beardifier pipeline vectors differ at line {i + 1}:\n"
                                     f"  native: {got}\n  fixture: {exp}")
            raise SystemExit(f"beardifier pipeline vector count differs: "
                             f"native {len(rows)} fixture {len(want)}")
    print(f"beardifier pipeline: {scenes} scenes / {samples} densities / {admits} admissions"
          + (" written" if a.output else " match"))


if __name__ == "__main__":
    main()
