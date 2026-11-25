#!/usr/bin/env python3
"""Generate the runtime block-registry asset pack from the pinned 1.21.1 server jar.

The block topology/gameplay metadata input (`blocks_1211.json`) is the pinned MIT
PrismarineJS minecraft-data file documented in NOTICE/THIRD_PARTY.md. Every binary
physical-fact table is measured from the user's own pinned vanilla server jar at install
time; no generated output is committed. The complete pack needs a JDK with both java
and javac on PATH: the mineshaft support probe is multi-source, and the pinned Java 21
runtime's source-file launcher cannot compile that shape without javac.

Usage:
  export_block_registry_assets.py --jar server.jar --blocks-json blocks.json --out-dir assets
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import zipfile

HERE = Path(__file__).resolve().parent
for candidate in (HERE, HERE.parent):
    if str(candidate) not in sys.path:
        sys.path.insert(0, str(candidate))

from vanilla_java import JAR_SHA1, pinned_runtime, require_jdk, run_probe  # noqa: E402
from block_predicate_oracle import encode as encode_predicates  # noqa: E402
from structure_transform_oracle import encode as encode_transforms  # noqa: E402

BLOCKS_SHA256 = "7d1d95c4e0255f9f269293e8077c7923c9ec0ff902a9a475ed131da8b26bc654"
REQUIRED_BLOCK_FIELDS = {
    "id", "name", "hardness", "resistance", "stackSize", "defaultState",
    "minStateId", "maxStateId", "states", "drops",
}
OUTPUTS = (
    "blocks_1211.json",
    "block_predicates_1211.bin",
    "block_state_facts_1211.bin",
    "collision_shape_full_1211.bin",
    "mineshaft_support_1211.bin",
    "multiface_support_1211.bin",
    "structure_state_transforms_1211.bin",
    "template_solid_1211.bin",
    "feature_order_1211.json",
)


def probe_source(repo_relative, flat_name):
    """Find a probe in the source tree or in the flat embedded installer layout."""
    candidates = (
        HERE.parent / repo_relative,
        HERE / repo_relative,
        HERE / flat_name,
    )
    for path in candidates:
        if path.is_file():
            return path
    raise FileNotFoundError(f"missing block-registry probe {flat_name}")


def atomic_write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_bytes(data)
    os.replace(tmp, path)


def validate_blocks(raw):
    digest = hashlib.sha256(raw).hexdigest()
    if digest != BLOCKS_SHA256:
        raise ValueError(
            "blocks json is not the pinned minecraft-data 1.21.1 file: sha256 "
            f"{digest}, expected {BLOCKS_SHA256}")
    blocks = json.loads(raw)
    if not isinstance(blocks, list) or len(blocks) != 1060:
        raise ValueError("pinned blocks json must contain exactly 1060 blocks")
    state_total = 0
    for block in blocks:
        missing = REQUIRED_BLOCK_FIELDS - set(block)
        if missing:
            raise ValueError(f"blocks json entry {block.get('name')!r} misses {sorted(missing)}")
        state_total += int(block["maxStateId"]) - int(block["minStateId"]) + 1
    if state_total != 26684:
        raise ValueError(f"pinned blocks json has {state_total} states, expected 26684")
    return blocks


def extract_templates(inner, target):
    target.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(inner)) as archive:
        prefix = "data/minecraft/structure/"
        count = 0
        for name in archive.namelist():
            if name.startswith(prefix) and name.endswith(".nbt"):
                rel = Path(name[len(prefix):])
                if rel.is_absolute() or ".." in rel.parts:
                    raise ValueError("unsafe structure path in server jar")
                out = target / rel
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_bytes(archive.read(name))
                count += 1
    if count != 1180:
        raise ValueError(f"server jar contains {count} structure templates, expected 1180")


def export_assets(jar, blocks_json, out_dir):
    require_jdk("block registry asset generation")
    raw_blocks = blocks_json.read_bytes()
    blocks = validate_blocks(raw_blocks)
    if hashlib.sha1(jar.read_bytes()).hexdigest() != JAR_SHA1:
        raise ValueError("not the pinned 1.21.1 server jar: " + str(jar))
    out_dir = out_dir.resolve()
    written = {}
    with pinned_runtime(jar) as (work, classpath, inner):
        predicates_out = work / "block-predicates.tsv"
        run_probe(work, classpath,
                  probe_source("block-predicate-oracle/BlockPredicateOracle.java",
                               "BlockPredicateOracle.java"),
                  "dev.megu.tools.BlockPredicateOracle", predicates_out)
        predicates, multiface, collision_full = encode_predicates(
            predicates_out.read_text(), blocks)
        written["block_predicates_1211.bin"] = predicates
        written["multiface_support_1211.bin"] = multiface
        written["collision_shape_full_1211.bin"] = collision_full

        facts_out = work / "block-state-facts.tsv"
        run_probe(work, classpath,
                  probe_source("block-state-facts-oracle/BlockStateFactsOracle.java",
                               "BlockStateFactsOracle.java"),
                  "dev.megu.tools.BlockStateFactsOracle", facts_out)
        # Reuse the facts encoder indirectly: it is keyed through the same native/wire
        # translation as the predicate encoder, but its row format is six columns.
        from block_state_facts_from_jar import encode as encode_facts
        written["block_state_facts_1211.bin"] = encode_facts(
            facts_out.read_text(), blocks)

        support_out = work / "mineshaft-support.tsv"
        run_probe(work, classpath,
                  probe_source("structure-oracle/MineshaftSupportOracle.java",
                               "MineshaftSupportOracle.java"),
                  "dev.megu.tools.MineshaftSupportOracle", support_out,
                  extra_sources=[probe_source("structure-oracle/NativeHarness.java",
                                              "NativeHarness.java")])
        mineshaft, _, _ = encode_predicates(support_out.read_text(), blocks)
        written["mineshaft_support_1211.bin"] = mineshaft

        templates = work / "templates"
        extract_templates(inner, templates)
        template_vectors, template_states = work / "templates.tsv", work / "template-states.tsv"
        run_probe(work, classpath,
                  probe_source("structure-oracle/StructureTemplateOracle.java",
                               "StructureTemplateOracle.java"),
                  "dev.megu.tools.StructureTemplateOracle",
                  templates, template_vectors, template_states)
        template_rows = "\n".join(
            line + "\t0\t0" for line in template_states.read_text().splitlines())
        template_solid, _, _ = encode_predicates(template_rows, blocks)
        written["template_solid_1211.bin"] = template_solid

        transform_vectors, transform_states = work / "transforms.tsv", work / "transform-states.tsv"
        run_probe(work, classpath,
                  probe_source("structure-oracle/StructureTransformOracle.java",
                               "StructureTransformOracle.java"),
                  "dev.megu.tools.StructureTransformOracle",
                  transform_vectors, transform_states)
        written["structure_state_transforms_1211.bin"] = encode_transforms(
            transform_states.read_text(), blocks)

        feature_order = work / "feature-order.json"
        run_probe(work, classpath,
                  probe_source("structure-oracle/FeatureOrderOracle.java",
                               "FeatureOrderOracle.java"),
                  "dev.megu.tools.FeatureOrderOracle", feature_order)
        written["feature_order_1211.json"] = feature_order.read_bytes()

    written["blocks_1211.json"] = raw_blocks
    missing = [name for name in OUTPUTS if name not in written]
    if missing:
        raise AssertionError("internal export omission: " + ", ".join(missing))
    expected_sizes = {
        "block_predicates_1211.bin": 26684,
        "block_state_facts_1211.bin": 26684 * 6,
        "collision_shape_full_1211.bin": 26684,
        "mineshaft_support_1211.bin": 26684,
        "multiface_support_1211.bin": 26684,
        "structure_state_transforms_1211.bin": 26684 * 12 * 4,
        "template_solid_1211.bin": 26684,
    }
    for name, size in expected_sizes.items():
        if len(written[name]) != size:
            raise ValueError(f"{name}: generated {len(written[name])} bytes, expected {size}")
    for name in OUTPUTS:
        atomic_write(out_dir / name, written[name])
    return {name: len(written[name]) for name in OUTPUTS}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, required=True)
    parser.add_argument("--blocks-json", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()
    sizes = export_assets(args.jar, args.blocks_json, args.out_dir)
    print("block registry assets exported: " + ", ".join(
        f"{name}={sizes[name]}" for name in OUTPUTS))


if __name__ == "__main__":
    main()
