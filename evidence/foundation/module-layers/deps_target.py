"""Design hypothesis for shared generation contracts, not an observed source graph.

Registry, provider and local-world-access contracts have independent owners now;
this file states the intended layering so the observation can be checked against it.
"""
import json
from pathlib import Path
import depsgx as d

layers = [
    ["main"],
    # The wire format is the tree's first consumer: it reads brigadier and nothing else, and
    # nothing below it may read it back. `seamless_portal` is the same shape: the
    # `megu:portal_seamless_v1` (MSP1) codec mounted from `megu_proto`, reading
    # only the leaf `wire` primitives.
    ["brigadier_wire", "seamless_portal"],
    ["chest_loot", "java_compat", "landing", "effects", "minecart", "spawner", "tick_persistence", "stream_gen", "tick_workers"],
    ["cached_lookup", "world_rng", "tick_queue", "landing_types", "flowing_fluid", "scheduled_ticks"],
    ["feature", "structure_template"],
    ["tree", "mineshaft"],
    ["generation_providers", "mineshaft_piece"],
    ["generation_context"],
    ["worldgen", "block_registry", "generated_content"],
    ["json", "generation_math", "generation_leaf_queue", "nbt", "generation_payload", "structure_transform", "structure_placement", "mineshaft_layout", "voxel_faces"],
    # Leaf utilities and profiling counters. Profilers read trace_env flags but
    # do not depend on generation or runtime owners. brigadier is a leaf library too: the
    # Brigadier port owns its own reader/tree/dispatcher and references no other crate module.
    ["ingress_guard", "tick_stats", "chunk_ack", "connection_limits", "trace_env",
     "chunkprof", "lockprof", "loopprof", "tickprof", "brigadier", "wire"],
]
modules = [name for layer in layers for name in layer]
graph = d.ModuleGraph(
    modules=modules,
    depends=[("main", name) for name in modules if name != "main"] + [
        ("brigadier_wire", "brigadier"),
        ("seamless_portal", "wire"),
        ("chest_loot", "generation_math"),
        ("spawner", "generation_math"), ("spawner", "nbt"),
        ("landing", "java_compat"), ("landing", "landing_types"),
        ("landing", "world_rng"),
        ("java_compat", "landing_types"),
        ("tick_persistence", "scheduled_ticks"),
        ("stream_gen", "feature"), ("stream_gen", "worldgen"),
        ("tick_workers", "block_registry"), ("tick_workers", "worldgen"),
        ("generated_content", "nbt"),
        ("structure_placement", "generation_math"),
        ("mineshaft_layout", "generation_math"),
        ("scheduled_ticks", "generation_math"),
        ("feature", "mineshaft"),
        *[("mineshaft", module) for module in (
            "generation_context", "generation_math", "json", "mineshaft_layout",
            "mineshaft_piece", "structure_placement", "worldgen")],
        *[("mineshaft_piece", module) for module in (
            "block_registry", "generation_context", "generation_math",
            "mineshaft_layout", "structure_transform", "generation_payload")],
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
