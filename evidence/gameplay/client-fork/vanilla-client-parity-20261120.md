# CF-1b Headless Differential Client Parity — 2026-11-20

## Tested inputs and method

- Repository source HEAD before this harness commit was `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`, branch `t3/vanilla-client-parity`. `cargo build --offline --locked` produced `target/debug/mc-spine`.
- Official Minecraft Java 1.21.1 `server.jar` at `/home/yuu/tmpx/carverfresh/server.jar`, verified SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, ran under `/usr/lib/jvm/java-21-openjdk/bin/java` with `-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData`.
- Both targets ran on loopback using Protocol **767**, an offline `ParityProbe` account, seed **4242**, creative mode, isolated disposable worlds, the same scripted player packets, and the same RCON arena commands. Both were set to peaceful difficulty and creative game mode through RCON before arena actions. The Rust server used `MC_SPINE_WG=old` flat terrain; Vanilla used `level-type=minecraft:flat`. Their initial terrain/spawn generation differs. The controlled arena was placed at matching absolute coordinates before action testing.
- Client frames, login/configuration handshake, chunk/light palette decoding and block-state IDs reuse the repository's Protocol 767 test implementation. The harness handles Vanilla's compression and relative-position flags, acknowledges teleports, and queries each server's authoritative player position over RCON after every scripted movement packet. No desktop client, authenticated Minecraft account, or externally hosted server was used.
- Comparison is deliberately strict. Each target must independently satisfy the scripted acceptance invariants. Any observed bootstrap packet-content or initial chunk-delivery difference marks its scenario `MISMATCH` even when required packets and decoding are present. Self-consistency runs two separate Rust server instances.

## Observed scenario results

| Scenario | Live mc-spine | Official Vanilla 1.21.1 | Result |
| --- | --- | --- | --- |
| Login → Configuration → Play, initial border `0x25`, advancements `0x74`, player position `0x40` | All required packets present; border parsed | All required packets present; border parsed | **MISMATCH** — advancement content, compression/login sequence, initial position |
| Chunk and light `0x27` decode | 25 decoded/active chunks, 24 sections, 4096 states per section, origin present | 45 decoded/active chunks, 24 sections, 4096 states per section, origin present | **MISMATCH** — 25 versus 45 initial chunks |
| Grounded flat walk | 6/6 accepted; end `(2.94, 240, 1.5)` | 6/6 accepted; same end | **PASS** — 0 correction `0x40` packets; max server-position delta `0.0` |
| Sprint | 6/6 accepted; end `(4.74, 240, 1.5)` | 6/6 accepted; same end | **PASS** — 0 corrections; delta `0.0` |
| Jump arc | 12/12 accepted; apex Y `241.252203` | 12/12 accepted; same apex | **PASS** — 0 corrections; delta `0.0` |
| Place stone | `0x09` state ID `1` at `(6, 240, 1)` | `0x09` state ID `1` at same position | **PASS** |
| Break stone | `0x09` state ID `0` at `(6, 240, 1)` | `0x09` state ID `0` at same position | **PASS** |
| Enter still-water column | 3/3 accepted; water state ID `80`; end `(9.5, 244.8, 2.5)` | 3/3 accepted; identical state/end | **PASS** — 0 corrections; delta `0.0` |
| Submerged scripted tick positions | 8/8 accepted; Y `243.5` → `243.40402847798026`; water state ID `80` | 8/8 accepted; same positions/state | **PASS** — 0 corrections; max per-step coordinate delta `0.0`, tolerance `0.03` blocks |

**Dual-target result — 7/9 PASS, 2 MISMATCH.** Both targets independently accepted all 35 movement packets (6 + 6 + 12 + 3 + 8) with zero server correction teleports and `0.0` maximum difference between requested and authoritative coordinates. The two block operations emitted the expected `0x09` updates on both servers. The Rust-only two-server self-consistency run passed **9/9**.

## Exact observed Vanilla divergences

**Bootstrap / `MISMATCH`**

| Observed field | mc-spine (all runs) | Vanilla run 1 | Vanilla run 2 | Vanilla run 3 (after common difficulty setup) |
| --- | --- | --- | --- | --- |
| Login-phase packet IDs | `0x02` | `0x03`, `0x02` | `0x03`, `0x02` | `0x03`, `0x02` |
| Compression threshold | absent (`null`) | `256` bytes (`0x03` Set Compression) | `256` bytes | `256` bytes |
| First advancements `0x74` body size | `1417` bytes | `274` bytes | `274` bytes | `274` bytes |
| First advancements `0x74` body SHA-256 | `6c1301ff4ed5d0b1dbb1f6b137f317b2cf84cee35160fc03a8d6275c5239e2b5` | `7b01b49088b6c686e4bb8fd22047681345e7043ad1510ae85ac0e342726800b9` | `bcfd147f49a497cc6913fcb6188314172c7813a32a4472acff89ba1ba8882bb0` | `4a4b85ee0fb5a559fc54549bf50f24bdfc345cd5fd9044300fb6887206562f54` |
| First initial player position `0x40` | `(0.5, 16.0, 0.5)` | `(7.5, -60.0, 7.5)` | `(3.5, -60.0, -3.5)` | `(-9.5, -60.0, 0.5)` |

Both `0x25` border payloads decode identically to center `(0,0)`, size `59999968`, maximum size `29999984`, warning distance `5`, warning time `15`, and zero interpolation duration. Both initial `0x40` frames had raw flags `0`. Both targets completed login and configuration. The three independently executed Vanilla sessions differed in their spawn coordinates and advancement-body hashes. The different initial player locations reflect the different flat-world generation configurations and spawn selection; later controlled movement uses identical arena coordinates. The advancement bodies genuinely differ in length and hash, and these runs do not establish whether their contents are semantically equivalent.

**Chunks / `MISMATCH`**

At the post-login observation point (after the origin chunk and an additional `0.6` seconds of packet reception), mc-spine had **25** decoded and active chunks while Vanilla had **45**. Each reported **24** decoded sections with **4096** states per section, a decoded `(0,0)` origin chunk, and **0** unmatched unloads. Chunk count is time- and initial-spawn-sensitive; the observation captures a concrete difference in delivered chunk counts at this point, without attributing its cause to the chunk codec.

## Reproduction and verification

The following commands were run from the worktree. `target/tmp` was configured before the build and both scripts, and it was used for isolated worlds and JVM temporary files.

```bash
mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"
cargo build --offline --locked
python3 tests/client_vanilla_parity.py
TMPDIR="$PWD/target/tmp" python3 tests/client_vanilla_parity.py --with-vanilla
bash tools/layers_check.sh
bash tools/debt_ratchet.sh
python3 tools/provenance_check.py
```

| Verification | Observed outcome |
| --- | --- |
| Offline locked Rust build | **PASS** |
| Rust self-consistency | **PASS**, 9/9 |
| Pinned official JVM comparison, three runs | **7/9 PASS, 2 MISMATCH** on each run, exit code `1` as designed for detected differences |
| `tools/layers_check.sh` | **PASS**, 44 modules, 111 edges, 0 named errors; existing named debt remains |
| `tools/debt_ratchet.sh` | **PASS**, 0 over limit |
| `tools/provenance_check.py` | **PASS**, provenance clean |

The fluid test checks that both live servers accept the same prescribed eight-step underwater displacement and remain within the **0.03 block** cross-server tolerance. It does not independently simulate full player fluid physics or prove that an unattended real client would generate that displacement. Likewise, matching movement acceptance after RCON fixture placement does not establish equivalence of the servers' original terrain, spontaneous spawn location, advancement definitions, or initial chunk-delivery cadence.


## E4-20 — Slab step-up, inventory crafting and combat

The expanded run used source base `d384fb8674eedf5b10d9b07464cf96912519e687` and debug binary SHA-256 `4d9d3406bb2582899a9ea73ed4d7d021549eb25a33e00aa426dbee5c82df8a6c`. The official 1.21.1 jar retained the pinned SHA-1 above and had SHA-256 `e3bc55693e93cda0188f2e60aea28113fc647c5e85a15fa3d1b347349231b4bb`. Both runs used Protocol 767 and seed 4242. The JVM was OpenJDK 21.0.13.1 with the temporary directory and flags documented above.

| Scenario | Live mc-spine | Official Vanilla 1.21.1 | Result |
| --- | --- | --- | --- |
| `slab_step_up` | Dry bottom slab state `11225`, 8/8 positions accepted, 0 unexpected `0x40`, final `(3.5, 240.5, 3.5)` | Same state, corrections and all eight positions | **PASS**, maximum position delta `0.0` blocks |
| `crafting_planks` | Consumed one oak log and stored four oak planks in slot 37, with `0x15` slot updates | Same recipe and final stack, with different `0x15` order and additional cursor updates | **MISMATCH** |
| `combat_hit_or_damage` | Health `20 → 19` on hit and `0` on `/kill`, with hurt angle `0.0` and velocity `(3000, 2500, 0)` | Same health changes, with hurt angle `179.99998474121094`, velocity `(3200, 2886, 0)`, a damage event and a death entity event | **MISMATCH** |

The slab fixture uses the same RCON `setblock 3 240 3 minecraft:stone_slab[type=bottom,waterlogged=false]` on both servers, then checks the block update and sends quarter-block horizontal steps. Reported Y rises by exactly `0.5` when the player's bounding box first overlaps the slab. The probe measures server acceptance of the scripted step. In an exploratory creative right-click placement at the same cell, mc-spine's cell remained air and Vanilla produced state `11225`.

The crafting trace sets up one log through RCON and sends four ordinary inventory clicks in survival. It follows the latest server menu state ID and decodes both full `0x13` and incremental `0x15` inventory updates. The measured crafting stages used `0x15` on both servers. Vanilla sent the craft output (slot 0) before the ingredient (slot 1) when the log entered the grid. Mc-spine sent the ingredient first. Vanilla also sent cursor slot `-1` with the lifted log and with the taken planks. Those two cursor updates were absent on mc-spine.

The combat trace used two survival players one block apart on the arena floor. It allowed Vanilla's 60-tick login invulnerability to expire, sent one bare-hand Interact attack, then used `/kill` for a separate death stimulus. Velocity values are the raw signed shorts from `0x5a`. Vanilla sent Damage Event `0x1a` with damage type ID `32` during the hit and Entity Event `0x1f` with event code `3` on death. Neither frame was observed for those actions on mc-spine. Protocol 767 assigns Disconnect to `0x1d`, Entity Event to `0x1f`, and Hurt Animation to `0x24`, as resolved from the pinned jar's play registration order using [Mojang's 1.21.1 server mappings](https://piston-data.mojang.com/v1/objects/03f8985492bda0afc0898465341eb0acef35f570/server.txt).

| Verification | Outcome |
| --- | --- |
| `cargo build --offline --locked` | Exit `0` |
| `python3 tests/client_vanilla_parity.py` | **12/12 PASS**, exit `0` |
| `python3 tests/client_vanilla_parity.py --with-vanilla` | **8/12 PASS, 4 MISMATCH**, exit `1` before shell masking |
| `bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py && git diff --check` | Exit `0` |

The original seven gameplay scenarios again matched with `0.0` position drift where measured. Bootstrap and initial chunk delivery again differed. In the capture below the initial spawn was `(0.5, 16.0, 0.5)` on mc-spine and `(4.5, -60.0, -5.5)` on Vanilla. The first advancements body was `1417` versus `274` bytes, compression was absent versus threshold `256`, login packet IDs were `[0x02]` versus `[0x03, 0x02]`, and the initial observation point saw `25` versus `45` decoded and active chunks.

### Full Vanilla differential JSON summary

```json
{
  "mc_spine": {
    "block_break": {"packet_0x09": true, "state": 0, "updates": [[[6, 240, 1], 0]]},
    "block_place": {"packet_0x09": true, "state": 1, "updates": [[[6, 240, 1], 1]]},
    "bootstrap": {"advancement_payload_bytes": [1417], "advancement_payload_sha256": ["6c1301ff4ed5d0b1dbb1f6b137f317b2cf84cee35160fc03a8d6275c5239e2b5"], "advancements_0x74": true, "border_0x25": true, "border_details": {"center": [0.0, 0.0], "lerp_time": 0, "max_size": 29999984, "new_size": 59999968.0, "old_size": 59999968.0, "warning_blocks": 5, "warning_time": 15}, "compression_threshold": null, "configuration_finish": true, "configuration_packet_ids": ["0x01", "0x0c", "0x0e", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x0d", "0x03"], "initial_position": [0.5, 16.0, 0.5], "initial_position_0x40": true, "join_game_0x2b": true, "login_packet_ids": ["0x02"], "login_success": true, "position_flags_raw": [0]},
    "chunks": {"chunks_active": 25, "chunks_decoded": 25, "decoded_0x27": true, "origin_chunk": true, "sections": 24, "states_per_section": 4096, "unmatched_unloads": 0},
    "combat_hit_or_damage": {"death_entity_events_0x1f": [], "death_health_0x5d": [0.0], "hit_damage_types_0x1a": [], "hit_entity_events_0x1f": [], "hit_health_0x5d": [19.0], "hit_hurt_angles_0x24": [0.0], "hit_velocity_0x5a": [[3000, 2500, 0]], "initial_health_0x5d": 20.0},
    "crafting_planks": {"slot_updates": {"given": [["0x15", 36, [132, 1]]], "lift_log": [["0x15", 36, [0, 0]]], "place_ingredient": [["0x15", 1, [132, 1]], ["0x15", 0, [36, 4]]], "store_planks": [["0x15", 37, [36, 4]]], "take_result": [["0x15", 0, [0, 0]], ["0x15", 1, [0, 0]]]}},
    "fluid_entry": {"accepted": 3, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 3, "server_positions": [[9.5, 245.1, 2.5], [9.5, 244.95, 2.5], [9.5, 244.8, 2.5]], "water_state": 80},
    "grounded_walk": {"accepted": 6, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 6, "server_positions": [[1.74, 240.0, 1.5], [1.98, 240.0, 1.5], [2.22, 240.0, 1.5], [2.46, 240.0, 1.5], [2.7, 240.0, 1.5], [2.94, 240.0, 1.5]]},
    "jump_arc": {"accepted": 12, "apex_y": 241.252203, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 12, "server_positions": [[4.74, 240.42, 1.5], [4.74, 240.7532, 1.5], [4.74, 241.00133599999998, 1.5], [4.74, 241.16610927999997, 1.5], [4.74, 241.24918709439999, 1.5], [4.74, 241.252203352512, 1.5], [4.74, 241.17675928546177, 1.5], [4.74, 241.02442409975254, 1.5], [4.74, 240.7967356177575, 1.5], [4.74, 240.49520090540233, 1.5], [4.74, 240.12129688729428, 1.5], [4.74, 240.0, 1.5]]},
    "slab_step_up": {"accepted": 8, "corrections_0x40": 0, "final_position": [3.5, 240.5, 3.5], "max_server_offset": 0.0, "sent": 8, "server_positions": [[1.75, 240.0, 3.5], [2.0, 240.0, 3.5], [2.25, 240.0, 3.5], [2.5, 240.0, 3.5], [2.75, 240.5, 3.5], [3.0, 240.5, 3.5], [3.25, 240.5, 3.5], [3.5, 240.5, 3.5]], "slab_state": 11225, "slab_updates": [11225]},
    "sprint": {"accepted": 6, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 6, "server_positions": [[3.24, 240.0, 1.5], [3.54, 240.0, 1.5], [3.84, 240.0, 1.5], [4.14, 240.0, 1.5], [4.44, 240.0, 1.5], [4.74, 240.0, 1.5]]},
    "submerged_ticks": {"accepted": 8, "corrections_0x40": 0, "first_y": 243.5, "last_y": 243.40402847798026, "max_server_offset": 0.0, "sent": 8, "server_positions": [[9.5, 243.5, 2.5], [9.5, 243.495, 2.5], [9.5, 243.4859999999404, 2.5], [9.5, 243.47379999978543, 2.5], [9.5, 243.45903999951602, 2.5], [9.5, 243.44223199912454, 2.5], [9.5, 243.423785598611, 2.5], [9.5, 243.40402847798026, 2.5]], "tick_tolerance": 0.03, "water_state": 80}
  },
  "mode": "vanilla-differential",
  "protocol": 767,
  "reference": {
    "block_break": {"packet_0x09": true, "state": 0, "updates": [[[6, 240, 1], 0]]},
    "block_place": {"packet_0x09": true, "state": 1, "updates": [[[6, 240, 1], 1]]},
    "bootstrap": {"advancement_payload_bytes": [274], "advancement_payload_sha256": ["9bf9a6ea87a49dd6c622143f7565a5df1e89d41fc2e8f51b762748bf8038456e"], "advancements_0x74": true, "border_0x25": true, "border_details": {"center": [0.0, 0.0], "lerp_time": 0, "max_size": 29999984, "new_size": 59999968.0, "old_size": 59999968.0, "warning_blocks": 5, "warning_time": 15}, "compression_threshold": 256, "configuration_finish": true, "configuration_packet_ids": ["0x01", "0x0c", "0x0e", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x07", "0x0d", "0x03"], "initial_position": [4.5, -60.0, -5.5], "initial_position_0x40": true, "join_game_0x2b": true, "login_packet_ids": ["0x03", "0x02"], "login_success": true, "position_flags_raw": [0]},
    "chunks": {"chunks_active": 45, "chunks_decoded": 45, "decoded_0x27": true, "origin_chunk": true, "sections": 24, "states_per_section": 4096, "unmatched_unloads": 0},
    "combat_hit_or_damage": {"death_entity_events_0x1f": [3], "death_health_0x5d": [0.0], "hit_damage_types_0x1a": [32], "hit_entity_events_0x1f": [], "hit_health_0x5d": [19.0], "hit_hurt_angles_0x24": [179.99998474121094], "hit_velocity_0x5a": [[3200, 2886, 0]], "initial_health_0x5d": 20.0},
    "crafting_planks": {"slot_updates": {"given": [["0x15", 36, [132, 1]]], "lift_log": [["0x15", 36, [0, 0]], ["0x15", -1, [132, 1]]], "place_ingredient": [["0x15", 0, [36, 4]], ["0x15", 1, [132, 1]]], "store_planks": [["0x15", 37, [36, 4]]], "take_result": [["0x15", 0, [0, 0]], ["0x15", 1, [0, 0]], ["0x15", -1, [36, 4]]]}},
    "fluid_entry": {"accepted": 3, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 3, "server_positions": [[9.5, 245.1, 2.5], [9.5, 244.95, 2.5], [9.5, 244.8, 2.5]], "water_state": 80},
    "grounded_walk": {"accepted": 6, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 6, "server_positions": [[1.74, 240.0, 1.5], [1.98, 240.0, 1.5], [2.22, 240.0, 1.5], [2.46, 240.0, 1.5], [2.7, 240.0, 1.5], [2.94, 240.0, 1.5]]},
    "jump_arc": {"accepted": 12, "apex_y": 241.252203, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 12, "server_positions": [[4.74, 240.42, 1.5], [4.74, 240.7532, 1.5], [4.74, 241.00133599999998, 1.5], [4.74, 241.16610927999997, 1.5], [4.74, 241.24918709439999, 1.5], [4.74, 241.252203352512, 1.5], [4.74, 241.17675928546177, 1.5], [4.74, 241.02442409975254, 1.5], [4.74, 240.7967356177575, 1.5], [4.74, 240.49520090540233, 1.5], [4.74, 240.12129688729428, 1.5], [4.74, 240.0, 1.5]]},
    "slab_step_up": {"accepted": 8, "corrections_0x40": 0, "final_position": [3.5, 240.5, 3.5], "max_server_offset": 0.0, "sent": 8, "server_positions": [[1.75, 240.0, 3.5], [2.0, 240.0, 3.5], [2.25, 240.0, 3.5], [2.5, 240.0, 3.5], [2.75, 240.5, 3.5], [3.0, 240.5, 3.5], [3.25, 240.5, 3.5], [3.5, 240.5, 3.5]], "slab_state": 11225, "slab_updates": [11225]},
    "sprint": {"accepted": 6, "corrections_0x40": 0, "max_server_offset": 0.0, "sent": 6, "server_positions": [[3.24, 240.0, 1.5], [3.54, 240.0, 1.5], [3.84, 240.0, 1.5], [4.14, 240.0, 1.5], [4.44, 240.0, 1.5], [4.74, 240.0, 1.5]]},
    "submerged_ticks": {"accepted": 8, "corrections_0x40": 0, "first_y": 243.5, "last_y": 243.40402847798026, "max_server_offset": 0.0, "sent": 8, "server_positions": [[9.5, 243.5, 2.5], [9.5, 243.495, 2.5], [9.5, 243.4859999999404, 2.5], [9.5, 243.47379999978543, 2.5], [9.5, 243.45903999951602, 2.5], [9.5, 243.44223199912454, 2.5], [9.5, 243.423785598611, 2.5], [9.5, 243.40402847798026, 2.5]], "tick_tolerance": 0.03, "water_state": 80}
  },
  "scenarios": {
    "block_break": {"differences": {}, "status": "PASS", "validation_failures": {}},
    "block_place": {"differences": {}, "status": "PASS", "validation_failures": {}},
    "bootstrap": {"differences": {"advancement_payload_bytes": {"mc_spine": [1417], "reference": [274]}, "advancement_payload_sha256": {"mc_spine": ["6c1301ff4ed5d0b1dbb1f6b137f317b2cf84cee35160fc03a8d6275c5239e2b5"], "reference": ["9bf9a6ea87a49dd6c622143f7565a5df1e89d41fc2e8f51b762748bf8038456e"]}, "compression_threshold": {"mc_spine": null, "reference": 256}, "initial_position": {"mc_spine": [0.5, 16.0, 0.5], "reference": [4.5, -60.0, -5.5]}, "login_packet_ids": {"mc_spine": ["0x02"], "reference": ["0x03", "0x02"]}}, "status": "MISMATCH", "validation_failures": {}},
    "chunks": {"differences": {"chunks_active": {"mc_spine": 25, "reference": 45}, "chunks_decoded": {"mc_spine": 25, "reference": 45}}, "status": "MISMATCH", "validation_failures": {}},
    "combat_hit_or_damage": {"differences": {"death_entity_events_0x1f": {"mc_spine": [], "reference": [3]}, "hit_damage_types_0x1a": {"mc_spine": [], "reference": [32]}, "hit_hurt_angles_0x24": {"mc_spine": [0.0], "reference": [179.99998474121094]}, "hit_velocity_0x5a": {"mc_spine": [[3000, 2500, 0]], "reference": [[3200, 2886, 0]]}}, "status": "MISMATCH", "validation_failures": {}},
    "crafting_planks": {"differences": {"slot_updates": {"mc_spine": {"given": [["0x15", 36, [132, 1]]], "lift_log": [["0x15", 36, [0, 0]]], "place_ingredient": [["0x15", 1, [132, 1]], ["0x15", 0, [36, 4]]], "store_planks": [["0x15", 37, [36, 4]]], "take_result": [["0x15", 0, [0, 0]], ["0x15", 1, [0, 0]]]}, "reference": {"given": [["0x15", 36, [132, 1]]], "lift_log": [["0x15", 36, [0, 0]], ["0x15", -1, [132, 1]]], "place_ingredient": [["0x15", 0, [36, 4]], ["0x15", 1, [132, 1]]], "store_planks": [["0x15", 37, [36, 4]]], "take_result": [["0x15", 0, [0, 0]], ["0x15", 1, [0, 0]], ["0x15", -1, [36, 4]]]}}}, "status": "MISMATCH", "validation_failures": {}},
    "fluid_entry": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 1e-05, "status": "PASS", "validation_failures": {}},
    "grounded_walk": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 1e-05, "status": "PASS", "validation_failures": {}},
    "jump_arc": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 1e-05, "status": "PASS", "validation_failures": {}},
    "slab_step_up": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 1e-05, "status": "PASS", "validation_failures": {}},
    "sprint": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 1e-05, "status": "PASS", "validation_failures": {}},
    "submerged_ticks": {"differences": {}, "max_position_delta": 0.0, "position_tolerance": 0.03, "status": "PASS", "validation_failures": {}}
  },
  "seed": 4242
}
```
