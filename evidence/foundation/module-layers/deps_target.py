"""The layering S2.2 is aiming for, checked before any code moves.

Today feature and tree form a cycle because feature.rs is two things at once: the block registry,
which the generation layers legitimately depend on, and the feature/config system, which generates
trees and therefore depends back on them. Splitting it in two points every edge downward.

The proposed split, lowest layer first:

    feature (registry: breg, name_to_state, is_solid, ...)
      -> worldgen          (terrain; needs block ids)
      -> tree              (tree shapes; needs terrain and block ids)
      -> featuredefs       (the config language and placement; generates trees)
      -> main

depsgx is the check: if this file reports no errors, the target is acyclic and the extraction is
worth starting. If it reports a violation, the split above is wrong and no code should move yet.
"""

import depsgx as d

graph = d.ModuleGraph(
    modules=["main", "featuredefs", "tree", "worldgen", "feature",
             "cached_lookup", "world_rng", "tick_queue", "landing", "effects", "java_compat"],
    depends=[
        ("main", "featuredefs"),
        ("main", "feature"),
        ("main", "cached_lookup"),
        ("main", "world_rng"),
        ("main", "tick_queue"),
        ("main", "landing"),
        ("main", "effects"),
        ("main", "java_compat"),
        # The config language parses tree configs and asks the tree layer to place them.
        ("featuredefs", "tree"),
        ("featuredefs", "worldgen"),
        ("featuredefs", "feature"),
        # The generation layers only ever reach down to the registry.
        ("tree", "worldgen"),
        ("tree", "feature"),
        ("worldgen", "feature"),
    ],
    roots=["main"],
    layers=[
        ["main"],
        ["java_compat", "landing", "effects"],
        ["cached_lookup", "world_rng", "tick_queue"],
        ["featuredefs"],
        ["tree"],
        ["worldgen"],
        ["feature"],
    ],
    forbidden=[],
)
