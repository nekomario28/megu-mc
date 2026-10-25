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
import os
from pathlib import Path
import subprocess
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/biome-tree/ExportBiomeTree.java"
JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"


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
    # Authenticate BEFORE extracting or invoking Java. The jar is executable reference code.
    payload = jar.read_bytes()
    if hashlib.sha1(payload).hexdigest() != JAR_SHA1:
        raise ValueError("not the pinned 1.21.1 server jar: " + str(jar))
    with tempfile.TemporaryDirectory(prefix="mc-biome-tree-") as tmp:
        work = Path(tmp)
        with zipfile.ZipFile(io.BytesIO(payload)) as outer:
            inner_name = "META-INF/versions/1.21.1/server-1.21.1.jar"
            inner = outer.read(inner_name)
            jars = [work / "server-game.jar"]
            jars[0].write_bytes(inner)
            # Flat destination names: no archive-controlled paths are extracted.
            for i, name in enumerate(sorted(outer.namelist())):
                if name.startswith("META-INF/libraries/") and name.endswith(".jar"):
                    target = work / f"library-{i}.jar"
                    target.write_bytes(outer.read(name))
                    jars.append(target)
        with zipfile.ZipFile(io.BytesIO(inner)) as archive:
            prefix = "data/minecraft/worldgen/biome/"
            biomes = {"minecraft:" + name[len(prefix):-5] for name in archive.namelist()
                      if name.startswith(prefix) and name.endswith(".json")}
        cp = os.pathsep.join(map(str, jars))
        subprocess.run(["javac", "-proc:none", "-cp", cp, "-d", tmp, str(SOURCE)], check=True, timeout=60)
        output = work / "tree.json"
        subprocess.run(["java", "-Xmx768m", "-cp", tmp + os.pathsep + cp,
                        "dev.mcspine.tools.ExportBiomeTree", str(output)], check=True, timeout=120, cwd=tmp)
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
    parser.add_argument("--jar", type=Path, default=Path(os.environ.get(
        "MC_SPINE_VANILLA_SERVER_JAR", str(Path.home() / ".cache/mc-spine-audit/load/vanilla/server.jar"))))
    parser.add_argument("--output", type=Path, default=ROOT / "datapacks/vanilla/multi_noise_biome_tree.json")
    args = parser.parse_args()
    try:
        export(args.jar, args.output)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"biome tree: {error}\n")


if __name__ == "__main__":
    main()
