"""A design hypothesis, not evidence of the current implementation's layering.

Separating registry, config and placement is the intended direction. Actual JSON,
RNG/context types and population calls must also acquire correct owners; moving
one function does not prove these edges or eliminate current source cycles.
"""
import json
from pathlib import Path
import depsgx as d

modules = ["main", "featuredefs", "tree", "worldgen", "feature", "cached_lookup",
           "world_rng", "tick_queue", "landing_types", "landing", "effects", "java_compat"]
graph = d.ModuleGraph(
    modules=modules,
    depends=[("main", name) for name in modules if name != "main"] + [
        ("featuredefs", "tree"),
        ("featuredefs", "worldgen"),
        ("featuredefs", "feature"),
        ("tree", "worldgen"),
        ("tree", "feature"),
        ("worldgen", "feature"),
        ("landing", "java_compat"),
        ("landing", "landing_types"),
        ("java_compat", "landing_types"),
    ],
    roots=["main"],
    layers=[
        ["main"],
        ["java_compat", "landing", "effects"],
        ["cached_lookup", "world_rng", "tick_queue", "landing_types"],
        ["featuredefs"], ["tree"], ["worldgen"], ["feature"],
    ],
    forbidden=[("worldgen", "tree")],
)
policy = json.loads((Path(__file__).with_name("policy.json")).read_text())
current = {name for layer in policy["layers"] for name in layer}
if not current.issubset(modules):
    raise ValueError(f"design omits classified source owners: {sorted(current - set(modules))}")
