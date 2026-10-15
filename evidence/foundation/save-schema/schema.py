"""The save format's sections and what they point at, written from the implementation.

Read from src/main.rs:
  - the world is a sparse overlay: per chunk (cx,cz), cells keyed by local_idx(x,y,z). A cell whose
    state equals base_state is REMOVED rather than stored, so silence means "the generator's answer".
  - MET4 holds mobs, the offline and live inventories, chests, furnaces and the scoreboard.
  - ARW2 holds projectiles. FAL2 holds falling-block entities (position, velocity, block state,
    age); the block itself has left the world, so a falling entity does NOT point at a world cell.
    FAL1 (the old cell list) is still readable and is converted on load.
  - chests, furnaces, signs and hoppers are keyed by WORLD POSITION, not by a cell id. hoppers are
    derived again on load from the block at the position; chests/furnaces/signs are stored.

The question is whether those references resolve. A position-keyed store whose cell is absent from
the overlay is an orphan: the container exists after a load but the block that owns it does not,
so it can never be opened or broken and its contents are unreachable. The world being sparse is what
makes that possible, and the save format has no field that would notice.
"""

import schemagx as s

schema = s.Schema(
    tables={
        "world_cell": {"chunk_x": "int", "chunk_z": "int", "local_idx": "int", "state": "int"},
        "mob": {"eid": "int", "etype": "int", "x": "float", "y": "float", "z": "float", "hp": "float"},
        "drop": {"eid": "int", "x": "float", "y": "float", "z": "float", "item": "int", "count": "int"},
        "chest": {"x": "int", "y": "int", "z": "int", "slots": "blob"},
        "furnace": {"x": "int", "y": "int", "z": "int", "input": "blob", "fuel": "blob", "output": "blob"},
        "sign": {"x": "int", "y": "int", "z": "int", "front": "bool", "lines": "text"},
        "falling": {"eid": "int", "x": "float", "y": "float", "z": "float", "dy": "float", "state": "int", "time": "int"},
        "arrow": {"eid": "int", "owner": "int", "x": "float", "y": "float", "z": "float"},
    },
    primary_keys={
        "world_cell": "local_idx",
        "mob": "eid",
        "drop": "eid",
        "arrow": "eid",
        "chest": "x",
        "furnace": "x",
        "sign": "x",
        "falling": "eid",
    },
    foreign_keys=[
        # The position-keyed stores point at the cell that owns them.
        ("chest.x", "world_cell.local_idx"),
        ("furnace.x", "world_cell.local_idx"),
        ("sign.x", "world_cell.local_idx"),
        # A falling entity's block has left the world, so it has no owning cell to point at.
    ],
    # The world is what everything else hangs off; nothing points back at it.
    roots=["world_cell"],
)
