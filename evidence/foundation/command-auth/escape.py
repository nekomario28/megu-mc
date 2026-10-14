"""The escape hatch, modelled on its own so the hole has a shape and a name.

MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1 makes run_command_ctx skip the allowlist for a player. That is
deliberate - the comment calls it an explicit override reserved for local compatibility testing, not
public or LAN deployments - but a deliberate hole is still a hole, and this file is where it is
written down in the form a checker reads.

authzgx is expected to report an escalation path here. That is the point: the finding is not a bug to
fix, it is the risk of the flag stated precisely and kept next to the model that has none.
"""

import authzgx as a

model = a.AuthModel(
    principals=["player", "player_with_escape_flag", "console"],
    roles=["guest", "operator"],
    permissions=[
        "help", "list", "time query",
        "time set", "setblock", "fill", "clone", "execute", "give", "summon", "kill",
        "tp", "gamemode", "effect", "enchant", "scoreboard", "tag", "team", "bossbar",
        "particle", "worldborder", "forceload", "locate", "recipe", "advancement",
        "attribute", "damage", "item", "place", "random", "reload", "spectate",
        "spreadplayers", "trigger", "waypoint", "data", "stop", "save-all", "op",
    ],
    grants=[
        ("guest", "help"), ("guest", "list"), ("guest", "time query"),
        ("operator", "time set"), ("operator", "setblock"), ("operator", "fill"),
        ("operator", "clone"), ("operator", "execute"), ("operator", "give"),
        ("operator", "summon"), ("operator", "kill"), ("operator", "tp"),
        ("operator", "gamemode"), ("operator", "effect"), ("operator", "enchant"),
        ("operator", "scoreboard"), ("operator", "tag"), ("operator", "team"),
        ("operator", "bossbar"), ("operator", "particle"), ("operator", "worldborder"),
        ("operator", "forceload"), ("operator", "locate"), ("operator", "recipe"),
        ("operator", "advancement"), ("operator", "attribute"), ("operator", "damage"),
        ("operator", "item"), ("operator", "place"), ("operator", "random"),
        ("operator", "reload"), ("operator", "spectate"), ("operator", "spreadplayers"),
        ("operator", "trigger"), ("operator", "waypoint"), ("operator", "data"),
        ("operator", "stop"), ("operator", "save-all"), ("operator", "op"),
    ],
    inherits=[],
    # The flag hands a player the operator role outright.
    assigned=[("player", "guest"), ("player_with_escape_flag", "operator"),
              ("console", "operator")],
    sensitive=["setblock", "fill", "clone", "execute", "give", "summon", "kill", "tp",
               "gamemode", "effect", "enchant", "op", "stop", "reload", "save-all"],
    low=["player", "player_with_escape_flag"],
)
