#!/usr/bin/env python3
"""Generate or verify stronghold + biome-aware ring vectors against 1.21.1.

The fixture (`tests/data/stronghold_1211.tsv`) carries two row kinds:

    stronghold  kind case seed cx cz attempts ground post_seed clip pieces writes sha rng_after payloads -
    rings       kind case seed -  -  distance spread count -  -  count  -    -    -          coords

Both arms come from `tools/structure-oracle/StrongholdOracle.java`: the stronghold
rows are produced by the native `StrongholdPieces` classes (weights, child
placement, `postProcess`) and the ring rows by native
`ChunkGeneratorStructureState.generateRingPositions` over a real Overworld
`MultiNoiseBiomeSource`, with the preferred-biome set taken from
`#minecraft:stronghold_biased_to` in the pinned datapack.
"""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests/data/stronghold_1211.tsv"
BIOME_TAG = ROOT / "datapacks/vanilla/data/minecraft/tags/worldgen/biome/stronghold_biased_to.json"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output", type=Path, help="write TSV to this path; otherwise verify the fixture")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, classpath, _):
        out = work / "stronghold.tsv"
        run_probe(
            work,
            classpath,
            ROOT / "tools/structure-oracle/StrongholdOracle.java",
            "dev.megu.tools.StrongholdOracle",
            out,
            BIOME_TAG,
            extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"],
        )
        text = out.read_text()
    if a.output:
        a.output.write_text(text)
        print(f"stronghold: wrote {len(text.splitlines())} vectors to {a.output}")
    else:
        if text != FIXTURE.read_text():
            raise SystemExit("stronghold vectors differ (no files changed)")
        print(f"stronghold: {len(text.splitlines())} vectors match")


if __name__ == "__main__":
    main()
