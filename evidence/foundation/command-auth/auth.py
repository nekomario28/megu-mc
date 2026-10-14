"""Command permissions as they ship, written from the implementation.

Read from src/main.rs:
  - command_allowed_for_player() is an ALLOWLIST: help, list, and `time query|get`. Everything else is
    refused with "You do not have permission to run that command".
  - run_command_ctx() applies it only when ctx.me is Some - a player. RCON and the console have no
    player context and reach every command.
  - allow_unsafe_player_admin() reads MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS; when it is "1" the
    allowlist is skipped entirely.

The allowlist shape is the safe one: a command that nobody thought about is refused, not granted. The
question this file asks is whether a player reaches anything sensitive through the shipped default,
and the escape is modelled separately in escape.py because it is a deliberate hole with a name.
"""

import authzgx as a

model = a.AuthModel(
    principals=["player", "console"],
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
    assigned=[("player", "guest"), ("console", "operator")],
    sensitive=[
        "setblock", "fill", "clone", "execute", "give", "summon", "kill", "tp",
        "gamemode", "effect", "enchant", "op", "stop", "reload", "save-all",
    ],
    low=["player"],
)
