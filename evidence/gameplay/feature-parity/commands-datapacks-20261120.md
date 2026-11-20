# E4-16 Command, RCON & Datapack Live Wire Boundary Receipt — 2026-11-20

## Provenance and test configuration

- Source base `0b5603f2fec915ce1927fdaba05cd36bdec1db5d` with current checkout head `736fa3d31807e71e1b40105e5a302e8bec2bfbc0`, branch `t3/command-datapack-wire-suite`.
- Protocol `767` (Java 1.21.1), executable `target/debug/mc-spine`, SHA-256 **`2a8682aaee0756128ee775b1b9fda1e7ca652eaf950d12e2ae20e158bcd2bf4d`**.
- `TMPDIR="$PWD/target/tmp"` throughout. The suite uses isolated temporary worlds, loopback TCP ports and a fresh RCON secret; it automatically terminates both server processes.
- Fixtures include `assets/`, `config_packets.bin`, `datapacks/vanilla`, copies of `spinetest`, `rectest`, `tagtest`, `modtest`, a synthetic `e4live` folder with function-load, tick, nested and scheduled functions, JSON recipe, item/block tags, and `ziponly.zip`.
- Important discovery fixture detail. The loader's `/datapack list` reports packs with indexed `.mcfunction` files. `vanilla` and recipe/tag-only packs receive isolated, no-op `wire_marker.mcfunction` files in the temporary fixtures, preserving their original data files without changing the repository. Their list presence therefore confirms function-indexed folder discovery, with this qualification.

## Executed validations

| Check | Result |
| --- | --- |
| `TMPDIR="$PWD/target/tmp" cargo build --offline --locked` | PASS. Three existing dead-code warnings (`spawn_etype_for_biome`, `choose_supported_hostile`, `biome_name_at_block`). |
| `TMPDIR="$PWD/target/tmp" cargo test --offline --locked -- command_tests brigadier` | PASS. Filtered main run **72 passed, 0 failed**. |
| `TMPDIR="$PWD/target/tmp" python3 tests/command_datapack_wire.py` | **81 passed, 0 failed**. Two fresh live servers, one with the default player command guard and one with explicit admin override. |
| `bash tools/layers_check.sh` | PASS. 44 source modules, 111 edges, 0 named errors. |
| `bash tools/debt_ratchet.sh` | PASS. 0 over limit. |
| `python3 tools/provenance_check.py` | PASS. Clean. |
| `git diff --check` | PASS. |

## Live feature parity and boundaries

| Surface | Live observation | Boundary or remaining gap |
| --- | --- | --- |
| Unpacked datapack discovery | Six folder-backed packs listed (`vanilla`, `spinetest`, `rectest`, `tagtest`, `modtest`, `e4live`) with temporary function markers where required. | Recipe/tag-only folders do not appear in `/datapack list` without a function to index. |
| Datapack lifecycle | `#e4:load` startup function created scoreboard and a diamond block, and `#e4:tick` scoreboard advanced across ticks. | Full datapack lifecycle parity is not established. |
| Functions and scheduling | `/function spine:hello`, synthetic nested `function`, and `/schedule function e4:scheduled 2t` executed with observable score/world changes. | Tests cover the exact functions listed below. |
| Recipes and tags | Existing `rectest` coal+stick crafting yielded four torches, consumed ingredients. Synthetic item-tag recipe yielded two torches and consumed coal. `tagtest` block tag and synthetic block tag matched live `execute if block` checks. | Other datapack JSON types are outside this suite. |
| Zipped datapack | `ziponly.zip` listed in `/datapack list`; `/function e4zip:probe` ran 1 command and placed `minecraft:emerald_block` at `26 80 24`. | Supported on `main` (`861f4c7`). |
| `/datapack enable`, `/datapack disable` | Both enable and disable packs (`Disabled datapack 'e4live'`, `Enabled datapack 'e4live'`) and reload active functions/recipes/tags. | Supported on `main` (`861f4c7`). |
| `/reload` | Returns `Reloading! (7 pack(s))` and refreshes active datapacks while keeping server live. | Supported on `main` (`861f4c7`). |
| RCON | TCP authentication rejects invalid password, accepts valid password and runs commands with reply packets. | Test uses loopback listener and synthetic secret. |
| Dispatcher and selectors | Live `execute as/at/positioned`, if/unless block/entity, store-result score, forceload add/query/remove, data block get/remove, scoreboard, tag, setblock, summon, kill, gamerule and RCON feedback passed. | Coverage is for the exact commands and selectors in the case list. |
| Dimension execution | `execute in minecraft:the_nether run setblock` reported a nether write. Cross-dimension `/tp` emitted actual 0x47 respawn packets both ways. | **`execute in ... if block` readback is currently wrong.** The condition uses `block_at()` (overworld) instead of dimension-scoped lookup and returns `Test failed` after the nether write. The test pins that observed boundary. |
| Default player permissions | Protocol 767 player ran `help`, `list`, `time query daytime`. Attempts to set time, place block, add an objective, list datapacks or reload returned `You do not have permission to run that command`. RCON readbacks confirmed the time, block and objective were unchanged, and server remained responsive. | Non-operator players receive explicit denial feedback (`861f4c7`). |
| Player admin override | In a separate server with `MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1`, a player ran `time set 1234` and read back its resulting time accounting for elapsed ticks. | Explicit test-only environment override, not a persistent OP role. |
| Live command tree packet | Observed initial Play sequence includes `0x10` (`ClientboundCommandsPacket`, Protocol 767) before initial position and first chunk batch. | Live `0x10` emitted on login (`861f4c7`). |

## Protocol 767 login capture

Raw Play packet IDs observed until the initial position packet (hex, in reception order):

```text
0x2b, 0x10, 0x53, 0x54, 0x56, 0x22, 0x22, 0x54, 0x0d, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x27, 0x2a, 0x2a, 0x2a, 0x2a, 0x0c, 0x64, 0x40
```

`0x10` appears on login in this bounded trace.

## Exact live regression cases

Executed `TMPDIR="$PWD/target/tmp" python3 tests/command_datapack_wire.py` with **81 PASS / 0 FAIL**. Each line below is an actual case label reported by this live execution.

```text
01. PASS rcon-rejects-invalid-password
02. PASS rcon-accepts-correct-password
03. PASS datapack-listed-vanilla
04. PASS datapack-listed-spinetest
05. PASS datapack-listed-rectest
06. PASS datapack-listed-tagtest
07. PASS datapack-listed-modtest
08. PASS datapack-listed-e4live
09. PASS zip-listed
10. PASS startup-load-function
11. PASS startup-load-world-effect
12. PASS tick-tag-repeats
13. PASS nested-mcfunction-dispatch
14. PASS nested-mcfunction-effect
15. PASS nested-function-world-effect
16. PASS schedule-2t-accepted
17. PASS scheduled-function-executed
18. PASS synthetic-block-tag-match
19. PASS existing-tagtest-block-tag-match
20. PASS existing-spinetest-function
21. PASS zip-function-executed
22. PASS zip-world-effect-present
23. PASS datapack-disable-live
24. PASS datapack-enable-live
25. PASS reload-live
26. PASS boundary-commands-keep-server-live
27. PASS loaded-recipe-registry
28. PASS protocol-767-login
29. PASS login-commands-packet-0x10-present
30. PASS player-allow-help
31. PASS player-allow-list
32. PASS player-allow-time-query-daytime
33. PASS player-deny-time-set
34. PASS player-deny-setblock
35. PASS player-deny-scoreboard
36. PASS player-deny-datapack
37. PASS player-deny-reload
38. PASS player-deny-time-unchanged
39. PASS player-deny-setblock-unchanged
40. PASS player-deny-scoreboard-unchanged
41. PASS player-deny-server-responsive
42. PASS rcon-help
43. PASS scoreboard-objective
44. PASS scoreboard-set
45. PASS execute-store-result-score
46. PASS execute-stored-score-readback
47. PASS setblock-live
48. PASS execute-if-block
49. PASS execute-unless-block
50. PASS execute-if-entity
51. PASS execute-unless-entity
52. PASS execute-positioned
53. PASS execute-at-selector
54. PASS execute-as-selector
55. PASS execute-in-nether
56. PASS execute-in-dimension-block-condition-boundary
57. PASS forceload-add
58. PASS forceload-query
59. PASS forceload-remove
60. PASS forceload-empty
61. PASS data-block-fixture
62. PASS data-block-get
63. PASS data-block-remove
64. PASS gamerule-set
65. PASS gamerule-readback
66. PASS summon-live
67. PASS selector-mob-type
68. PASS kill-mob-selector
69. PASS tag-player-add
70. PASS tag-player-readback
71. PASS tag-player-remove
72. PASS tp-cross-dimension-command
73. PASS tp-cross-dimension-respawn-0x47
74. PASS tp-return-overworld-command
75. PASS tp-return-overworld-respawn-0x47
76. PASS rectest-shapeless-crafting-result
77. PASS rectest-crafting-consumes-ingredients
78. PASS synthetic-item-tag-crafting-result
79. PASS synthetic-item-tag-crafting-consumed
80. PASS player-admin-override-command-allowed
81. PASS player-admin-override-readback
```

All test input worlds are ephemeral under `target/tmp`. No production world or external server was used. The suite does not claim Mojang/Vanilla reference-server parity for commands or datapacks beyond the listed observed behavior.
