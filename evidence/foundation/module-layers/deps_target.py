"""Design hypothesis for shared generation contracts, not an observed source graph.

Registry, provider and local-world-access contracts have independent owners now;
this file states the intended layering so the observation can be checked against it.
"""
import json
from pathlib import Path
import depsgx as d

layers = [
    ["main"],
    ["java_compat", "landing", "effects", "tick_persistence"],
    ["cached_lookup", "world_rng", "tick_queue", "landing_types", "flowing_fluid", "scheduled_ticks", "structure_placement"],
    ["feature", "structure_template"],
    ["tree"],
    ["generation_providers"],
    ["generation_context"],
    ["worldgen", "block_registry"],
    ["json", "generation_math", "generation_leaf_queue", "nbt", "staged_block_entity", "structure_transform", "voxel_faces"],
    # Leaf utilities and profiling counters. Profilers read trace_env flags but
    # do not depend on generation or runtime owners.
    ["ingress_guard", "tick_stats", "chunk_ack", "connection_limits", "trace_env",
     "chunkprof", "lockprof", "loopprof", "tickprof"],
]
modules = [name for layer in layers for name in layer]
graph = d.ModuleGraph(
    modules=modules,
    depends=[("main", name) for name in modules if name != "main"] + [
        ("landing", "java_compat"), ("landing", "landing_types"),
        ("landing", "world_rng"),
        ("java_compat", "landing_types"),
        ("tick_persistence", "scheduled_ticks"),
        ("structure_placement", "generation_math"),
        ("structure_template", "nbt"), ("structure_template", "block_registry"),
        ("structure_template", "structure_transform"), ("structure_template", "json"),
        ("structure_template", "generation_context"), ("structure_template", "generation_math"),
        ("structure_template", "voxel_faces"), ("tree", "voxel_faces"),
        ("feature", "structure_template"), ("feature", "structure_transform"), ("feature", "nbt"),
        ("feature", "tree"), ("feature", "worldgen"),
        ("feature", "block_registry"), ("feature", "generation_providers"),
        ("feature", "generation_context"), ("feature", "json"), ("feature", "generation_math"),
        ("tree", "block_registry"), ("tree", "generation_providers"),
        ("tree", "generation_context"), ("tree", "json"), ("tree", "generation_math"),
        ("tree", "generation_leaf_queue"),
        ("generation_providers", "block_registry"), ("generation_providers", "generation_context"),
        ("generation_providers", "json"), ("generation_providers", "generation_math"),
        ("generation_context", "worldgen"), ("generation_context", "block_registry"),
        ("generation_context", "generation_math"),
        ("block_registry", "json"),
        ("worldgen", "json"), ("worldgen", "generation_math"),
        ("worldgen", "block_registry"),
        ("feature", "trace_env"), ("worldgen", "trace_env"),
        ("effects", "tickprof"),
    ],
    roots=["main"],
    layers=layers,
    forbidden=[("worldgen", "tree")],
)
policy = json.loads((Path(__file__).with_name("policy.json")).read_text())
current = {name for layer in policy["layers"] for name in layer}
if not current.issubset(modules):
    raise ValueError(f"design omits classified source owners: {sorted(current - set(modules))}")
