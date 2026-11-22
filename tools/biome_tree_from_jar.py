#!/usr/bin/env python3
"""Export the pinned 1.21.1 climate RTree; never construct it from a newer preset.

The parameter-list JSON in the jar contains only a preset name. Its intervals and
RTree are built by vanilla code. Horizon's VanillaBiomeExport suggested calling
that builder rather than maintaining a second copy (see horizon-reuse evidence).
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import zipfile

from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/biome-tree/ExportBiomeTree.java"


def validate_tree(tree, biomes):
    """Fail closed on empty, partial or foreign-version exported trees."""
    if set(tree) != {"overworld", "nether"}:
        raise ValueError("expected exactly overworld and nether presets")
    counts = {}

    def walk(node):
        parameters = node["parameters"]
        if len(parameters) != 7:
            raise ValueError("expected seven climate intervals")
        for interval in parameters:
            low, high = interval["min"], interval["max"]
            if type(low) is not int or type(high) is not int or not -(2**31) <= low <= high < 2**31:
                raise ValueError("invalid quantized climate interval")
        if node["_type"] == "leaf":
            if node["biome"] not in biomes:
                raise ValueError("biome absent from pinned jar: " + node["biome"])
            return 1
        if node["_type"] != "branch" or not node["subTree"]:
            raise ValueError("empty or unknown climate node")
        return sum(walk(child) for child in node["subTree"])

    for preset, node in tree.items():
        counts[preset] = walk(node)
    if counts["overworld"] < 100 or counts["nether"] != 5:
        raise ValueError("incomplete climate export: " + repr(counts))
    return counts


def export(jar, destination):
    with pinned_runtime(jar) as (work, cp, inner):
        with zipfile.ZipFile(io.BytesIO(inner)) as archive:
            prefix = "data/minecraft/worldgen/biome/"
            biomes = {"minecraft:" + name[len(prefix):-5] for name in archive.namelist()
                      if name.startswith(prefix) and name.endswith(".json")}
        output = work / "tree.json"
        run_probe(work, cp, SOURCE, "dev.megu.tools.ExportBiomeTree", output)
        tree = json.loads(output.read_text())
        counts = validate_tree(tree, biomes)
        tree["_provenance"] = {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
                               "exporter_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest()}
        # Never replace the old tree if compilation, the oracle, or validation failed.
        destination.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(mode="w", dir=destination.parent, delete=False) as handle:
            staged = Path(handle.name)
            try:
                json.dump(tree, handle, separators=(",", ":"), sort_keys=True)
                handle.write("\n")
            except BaseException:
                staged.unlink(missing_ok=True)
                raise
        try:
            staged.replace(destination)
        finally:
            staged.unlink(missing_ok=True)
        print(f"biome tree: {counts}, pinned jar {JAR_SHA1} -> {destination}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--output", type=Path, default=ROOT / "datapacks/vanilla/multi_noise_biome_tree.json")
    args = parser.parse_args()
    try:
        export(args.jar, args.output)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"biome tree: {error}\n")


if __name__ == "__main__":
    main()
