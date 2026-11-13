#!/usr/bin/env python3
"""Verify mineshaft admission, caller slots, Mesa adjustment and authored writer scenes."""
import argparse
import json
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests/data/mineshaft_pipeline_1211.json"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output", type=Path, help="explicit destination; otherwise compare without writing")
    p.add_argument("--missing-tags-control", action="store_true")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, classpath, _):
        output = work / "mineshafts.json"
        args = [output] + (["missing-tags-control"] if a.missing_tags_control else [])
        run_probe(work, classpath, ROOT / "tools/structure-oracle/MineshaftPipelineOracle.java",
                  "dev.mcspine.tools.MineshaftPipelineOracle", *args, extra_sources=[
                      ROOT / "tools/structure-oracle/NativeHarness.java",
                      ROOT / "tools/structure-oracle/MineshaftLayoutOracle.java"])
        data = json.loads(output.read_text())
    if a.output:
        a.output.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    elif data != json.loads(FIXTURE.read_text()):
        raise SystemExit("mineshaft pipeline vectors differ (no files changed)")
    print(f"mineshaft pipeline: {len(data['cases'])} admission grids / "
          f"{len(data['writers'])} authored scenes " + ("written" if a.output else "match"))


if __name__ == "__main__":
    main()
