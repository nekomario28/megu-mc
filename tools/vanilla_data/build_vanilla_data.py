#!/usr/bin/env python3
"""Turn what Extract.java read out of the vanilla 1.21.1 server into mc-spine's data files,
after checking it against the data generator's own reports.

  build_vanilla_data.py <extract-dir> <datagen-reports-dir> <repo-root> <jar-sha1>

Reads   <extract-dir>/{state_facts.tsv,biome_tree.json,biome_probe.tsv,
        biome_probe_ties.tsv,random_next_int.tsv}   (Extract.java)
        <datagen-reports-dir>/{blocks.json,biome_parameters/minecraft/*.json}  (vanilla --reports)
Writes  assets/block_states_1211.json        per-state light / flag / face facts, our schema
        datapacks/vanilla/biome_climate_tree.json   the game's own biome search trees
        tests/data/biome_probe_1211.tsv      points looked up through the game's tree
        tests/data/biome_probe_ties_1211.tsv tie-heavy points (range edges) for the scan order
        tests/data/random_next_int_1211.tsv  nextInt(bound) streams of the game's random classes

Every check below must pass or nothing is written.
"""
import json, struct, sys
from collections import Counter
from pathlib import Path

ex, reports, root, jar_sha1 = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4]

# ---- block states ---------------------------------------------------------------------------
rows = [l.rstrip("\n").split("\t") for l in (ex / "state_facts.tsv").read_text().splitlines()]
report = json.loads((reports / "blocks.json").read_text())
id_to_name = {}
for name, b in report.items():
    for s in b["states"]:
        id_to_name[s["id"]] = name
n = len(rows)
assert n == len(id_to_name), f"game has {n} block states, the data generator reports {len(id_to_name)}"
for r in rows:
    assert id_to_name[int(r[0])] == r[1], f"state {r[0]}: game says {r[1]}, report says {id_to_name[int(r[0])]}"
# run-length list of blocks, so a loader can confirm the numbering against its own block table
blocks, start = [], 0
for i in range(1, n + 1):
    if i == n or rows[i][1] != rows[start][1]:
        blocks.append([rows[start][1], start, i - start])
        start = i
assert len({b[0] for b in blocks}) == len(blocks), "a block's states are not contiguous"
assert len(blocks) == len(report)

col = lambda k: [int(r[k]) for r in rows]
# state_facts.tsv columns: id, block, luminance, opacity, flags, sides
# flags (Extract.java): 1 air, 2 replaceable, 4 liquid(), 8 solid, 16 holds a fluid
flags = col(4)
facts = {
    "format": 1,
    "minecraft": "1.21.1",
    "jar_sha1": jar_sha1,
    "generated_by": "tools/vanilla_data/gen_vanilla_data.sh",
    "about": "Per block state, as the vanilla 1.21.1 server answers it once its registries are "
             "bootstrapped. Indexed by state id; `blocks` is [name, first_state_id, state_count] "
             "and must agree with the block table. Not derived from any other implementation.",
    "flag_bits": {"air": 1, "replaceable": 2, "liquid": 4, "solid": 8, "fluid": 16},
    "flag_meaning": {
        "air": "BlockState.isAir()",
        "replaceable": "BlockState.isReplaceable()",
        "liquid": "BlockState.isLiquid() - water, lava, bubble column",
        "solid": "BlockState.isSolid() (the legacy solid flag)",
        "fluid": "the state holds a non-empty fluid (water, lava, waterlogging, kelp, seagrass)",
    },
    "side_order": ["down", "up", "north", "south", "west", "east"],
    "side_meaning": "bit d set: isSideSolid(EmptyBlockView, origin, side d, SideShapeType.FULL)",
    "blocks": blocks,
    "luminance": col(2),
    "light_block": col(3),
    "flags": flags,
    "full_sides": col(5),
}
assert max(facts["luminance"]) <= 15 and max(facts["light_block"]) <= 15

# ---- biome trees ----------------------------------------------------------------------------
tree = json.loads((ex / "biome_tree.json").read_text())

def leaves(node, out):
    if node["_type"] == "leaf":
        out.append((node["biome"], tuple((p["min"], p["max"]) for p in node["parameters"])))
    else:
        for c in node["subTree"]:
            leaves(c, out)
    return out

def q(v):
    # Climate.quantizeCoord: (long)(float * 10000.0F), in float arithmetic
    f = struct.unpack("f", struct.pack("f", v))[0]
    return int(struct.unpack("f", struct.pack("f", f * 10000.0))[0])

def rng(v):
    return (q(v[0]), q(v[1])) if isinstance(v, list) else (q(v), q(v))

order = ["temperature", "humidity", "continentalness", "erosion", "depth", "weirdness", "offset"]
for dim in ("overworld", "nether"):
    flat = []
    for e in json.loads((reports / "biome_parameters" / "minecraft" / f"{dim}.json").read_text())["biomes"]:
        flat.append((e["biome"], tuple(rng(e["parameters"][k]) for k in order)))
    got = leaves(tree[dim], [])
    assert Counter(flat) == Counter(got), f"{dim}: the game's tree leaves differ from the data generator's parameter list"
    print(f"{dim}: {len(got)} leaves match the data generator's parameter list")

def check_bounds(node):
    # every branch encloses its children, as the game's builder guarantees
    if node["_type"] == "leaf":
        return
    for c in node["subTree"]:
        for i in range(7):
            assert node["parameters"][i]["min"] <= c["parameters"][i]["min"]
            assert node["parameters"][i]["max"] >= c["parameters"][i]["max"]
        check_bounds(c)
for dim in tree:
    check_bounds(tree[dim])

# ---- write ----------------------------------------------------------------------------------
def dump_compact(obj):
    # arrays of numbers on one line each; everything else on its own lines
    out = ["{"]
    items = list(obj.items())
    for i, (k, v) in enumerate(items):
        comma = "," if i + 1 < len(items) else ""
        if isinstance(v, list) and v and isinstance(v[0], (int, list)):
            body = "[\n" + ",\n".join(json.dumps(x, separators=(",", ":")) for x in v) + "\n]" if isinstance(v[0], list) \
                else json.dumps(v, separators=(",", ":"))
            out.append(f"{json.dumps(k)}:{body}{comma}")
        else:
            out.append(f"{json.dumps(k)}:{json.dumps(v)}{comma}")
    out.append("}")
    return "\n".join(out) + "\n"

(root / "assets").mkdir(exist_ok=True)
(root / "assets" / "block_states_1211.json").write_text(dump_compact(facts))
(root / "datapacks" / "vanilla").mkdir(parents=True, exist_ok=True)
(root / "datapacks" / "vanilla" / "biome_climate_tree.json").write_text(json.dumps(tree, separators=(",", ":")) + "\n")
(root / "tests" / "data").mkdir(parents=True, exist_ok=True)
(root / "tests" / "data" / "biome_probe_1211.tsv").write_text((ex / "biome_probe.tsv").read_text())
# Small reference vectors for the differential tests (numbers only, read from the running game).
(root / "tests" / "data" / "biome_probe_ties_1211.tsv").write_text((ex / "biome_probe_ties.tsv").read_text())
(root / "tests" / "data" / "random_next_int_1211.tsv").write_text((ex / "random_next_int.tsv").read_text())
print("wrote assets/block_states_1211.json, datapacks/vanilla/biome_climate_tree.json, tests/data/biome_probe_1211.tsv,")
print("      tests/data/biome_probe_ties_1211.tsv, tests/data/random_next_int_1211.tsv")
