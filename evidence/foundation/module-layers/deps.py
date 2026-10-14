"""mc-spine's module graph, measured from `crate::` references, against the intended layering.

Edges are what the source does today. The layers are what S2.2 needs before main.rs can be split:
extracting another 360 lines into a module is only safe if the target direction already holds,
otherwise the new module joins the cycle and every later move is harder.
"""

import depsgx as d

graph = d.ModuleGraph(
    modules=["main", "java_compat", "landing", "effects", "cached_lookup", "world_rng",
             "feature", "tree", "worldgen"],
    # Measured: feature -> tree, feature -> worldgen, tree -> feature, tree -> worldgen,
    # worldgen -> feature, main -> feature.
    depends=[
        ("main", "feature"),
        ("feature", "tree"),
        ("feature", "worldgen"),
        ("tree", "feature"),
        ("tree", "worldgen"),
        ("worldgen", "feature"),
    ],
    roots=["main"],
    # Intended, lowest first: nothing here may reach upward.
    layers=[
        ["main"],
        ["java_compat", "landing", "effects"],
        ["cached_lookup", "world_rng"],
        ["feature"],
        ["tree", "worldgen"],
    ],
    # The registry is the owner of block knowledge; world generation must not reach into the
    # tree/feature layer to ask it questions.
    forbidden=[("worldgen", "tree")],
)
