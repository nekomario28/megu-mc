# SPDX-License-Identifier: LGPL-3.0-or-later
#!/usr/bin/env python3
"""Static audit for generation payload staging and dimension materialization."""

import argparse
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def findings():
    feature = read("src/feature.rs")
    main = read("src/main.rs")
    save = read("src/save_v2.rs")
    single = read("src/single_piece_structures.rs")
    template = read("src/structure_template.rs")
    ocean = read("src/template_structures.rs")
    problems = []

    for path, source in (("src/feature.rs", feature), ("src/single_piece_structures.rs", single),
                         ("src/template_structures.rs", ocean), ("src/structure_template.rs", template)):
        for match in re.finditer(r"(?:let|const)\s+_\w*(?:loot_seed|mob)\w*\s*=", source):
            problems.append(f"{path}:{source.count(chr(10), 0, match.start()) + 1}: discarded loot/spawner draw")
    if "let loot_seed = rng.next_i64();" not in feature or "Some(loot_seed)" not in feature:
        problems.append("monster-room loot seed is not carried into a payload")
    if "let mob = rng.next_bound(4) as usize;" not in feature or "ctx.push_spawner(pos, mob_id)" not in feature:
        problems.append("monster-room mob draw is not staged as a spawner entity")

    drain = main[main.find("fn materialize_pending_generation"):main.find("fn generated_content", main.find("fn materialize_pending_generation"))]
    if "for dim in DimId::ALL" not in drain or (
        "with_gen_dim(dim" not in drain
        and "take_generation_payloads_for_dim(dim)" not in drain
    ):
        problems.append("pending generation drain does not cover all registered vanilla dimensions")
    if re.search(r"with_gen\s*\(\s*\|g\|\s*g\.take_staged_payloads", main + save):
        problems.append("an Overworld-only staged-payload drain remains")
    save_flush = save[save.find("fn save_world_into_impl"):save.find("let mut content =", save.find("fn save_world_into_impl"))]
    if "for dim in DimId::ALL" not in save_flush or "materialize_generation_payloads_for_dim(dim, pending)" not in save_flush:
        problems.append("save-time staged payload flush does not preserve all three dimensions")
    materializer = main[main.find("fn materialize_generation_payloads_for_dim"):main.find("fn roll_cart_loot")]
    if "DimId::Overworld" in materializer:
        problems.append("dimension-aware materializer still hardcodes Overworld")
    if "GenerationPayload::Brushable" not in materializer or "materialize_generated_brushable(dim" not in materializer:
        problems.append("brushable payload is not materialized into the live block-entity store")

    checks = {
        "desert_well": bool(re.search(r"ctx\.push_brushable\(\s*first", feature))
        and bool(re.search(r"ctx\.push_brushable\(\s*second", feature)),
        "desert_pyramid": "LootKind::Brushable =>" in single and "ctx.push_brushable(loot.pos" in single,
        "ocean_ruin": "minecraft:archaeology/ocean_ruin_warm" in ocean and "minecraft:archaeology/ocean_ruin_cold" in ocean
        and "GenerationPayload::Brushable" in template,
        "template suspicious gravel": '"minecraft:suspicious_sand" | "minecraft:suspicious_gravel"' in template
        and "GenerationPayload::Brushable { loot_table, loot_seed }" in template,
    }
    for name, ok in checks.items():
        if not ok:
            problems.append(f"{name} archaeology payload path is not wired to Brushable")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--strict", action="store_true", help="exit nonzero on any missing wiring")
    args = parser.parse_args()
    problems = findings()
    if problems:
        print("payload wiring audit: FAIL")
        for problem in problems:
            print(f"- {problem}")
        return 1 if args.strict else 0
    print("payload wiring audit: PASS (dimension drains, entity payloads, archaeology routes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
