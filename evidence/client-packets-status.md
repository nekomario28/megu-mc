# Protocol 767 clientbound Play packet status

Scope: packets reported missing by `tools/v767_total_parity_audit.py` at the
start of the client packet coverage task. The audit is static source evidence;
it does not prove wire correctness or runtime reachability. After the changes,
the audit reports 115/124 Play/Clientbound packets covered, up from 73/124.

`decoded` means the 767 packet body is parsed into typed fields. `dispatched`
means an existing subsystem receives it. `stored` means the client retains
bounded/minimal state because a renderer or UI consumer is absent. `deferred`
means no production decoder is present.

| Packet | Status | Handling |
|---|---|---|
| `bundle_delimiter` | deferred | Packet grouping/bundle dispatch is not implemented. |
| `add_experience_orb` | deferred | Experience-orb entity spawn is not represented by the current entity model. |
| `animate` | decoded, dispatched, stored | Updates the tracked entity's latest animation id; no hand-swing renderer consumes it yet. |
| `block_changed_ack` | decoded, stored | Sequence id is retained in the bounded pending UI/event queue; no acknowledgement UI exists. |
| `block_event` | decoded, dispatched, stored | World retains the last 64 block-event tuples for a future piston/chest renderer. |
| `change_difficulty` | decoded, stored | Difficulty id and lock flag retained in `ProtocolClient`. |
| `clear_titles` | decoded, stored | Typed event retained for the future title UI. |
| `command_suggestions` | decoded, stored | Transaction/range/matches parsed; latest result retained for a future command UI. |
| `commands` | deferred | Brigadier argument parser property codecs are not implemented; raw opaque bytes are not claimed as a decode. |
| `cookie_request` | decoded, dispatched | Play request key parsed; responds with an in-memory cookie when stored, otherwise an empty response. |
| `cooldown` | decoded, stored | Item id and cooldown ticks retained in a bounded map. |
| `custom_chat_completions` | decoded, stored | Add/remove/set actions update a bounded in-memory completion list. |
| `debug_sample` | deferred | Sample payload variants are not decoded. |
| `delete_chat` | decoded, dispatched | Signature or indexed deletion parsed and applied to matching bounded chat-log entries. |
| `disguised_chat` | decoded, dispatched, stored | Component text extracted and appended to the bounded chat log. |
| `explode` | deferred | Particle, block interaction, sound, and motion layout/consumers are not implemented. |
| `move_vehicle` | decoded, stored | Latest vehicle pose retained; mounted-player physics does not consume it yet. |
| `open_book` | decoded, stored | Hand parsed and event queued for a future book UI. |
| `open_sign_editor` | decoded, stored | Position and front/back side parsed and event queued for a future sign editor. |
| `ping` | decoded, dispatched | Fixed-width ping id parsed and returned with the Play `pong` packet. |
| `place_ghost_recipe` | decoded, stored | Window id and recipe id parsed; no recipe-placement UI exists. |
| `player_abilities` | decoded, stored | Flags and flying/walking speeds retained in `ProtocolClient`. |
| `player_chat` | decoded, dispatched, stored | Signed-chat envelope fields parsed; message text, chain index, and signature enter the bounded chat log. |
| `player_combat_end` | decoded, stored | Duration parsed and event queued for future combat HUD handling. |
| `player_combat_enter` | decoded, stored | Event queued for future combat HUD handling. |
| `player_combat_kill` | decoded, stored | Player id and anonymous-NBT message parsed; event queued for future combat HUD handling. |
| `player_info_remove` | decoded, stored | UUID list parsed by the existing decoder and queued for future tab-list state handling. |
| `player_look_at` | decoded, stored | Target/anchor parsed by the existing decoder and queued; camera control does not consume it yet. |
| `recipe` | deferred | Recipe packet body variants are not decoded. |
| `resource_pack_pop` | decoded, stored | Optional UUID parsed and event queued; resource-pack stack/application is absent. |
| `resource_pack_push` | decoded, stored | UUID, URL, hash, forced flag, and optional prompt component parsed; downloading/applying packs is absent. |
| `set_border_warning_delay` | decoded, stored | Warning time retained in `ProtocolClient`. |
| `set_border_warning_distance` | decoded, stored | Warning distance retained in `ProtocolClient`. |
| `set_camera` | decoded, stored | Entity id parsed and queued for a future camera controller. |
| `set_carried_item` | decoded, stored | Selected hotbar slot retained in `ProtocolClient`. |
| `set_chunk_cache_radius` | decoded, stored | Requested radius retained; current loaded chunk policy is not resized from it. |
| `set_default_spawn_position` | decoded, stored | Packed position and angle retained in `ProtocolClient`. |
| `set_entity_link` | decoded, dispatched, stored | Source-to-destination attachment retained in the world entity-link map. |
| `set_passengers` | decoded, dispatched, stored | Vehicle passenger ids retained in the world passenger map. |
| `set_simulation_distance` | decoded, stored | Requested distance retained in `ProtocolClient`. |
| `start_configuration` | deferred | Mid-play phase transition and configuration handshake are not implemented. |
| `store_cookie` | decoded, dispatched, stored | Key and bounded payload parsed; at most 256 in-memory values, each capped at 5 KiB. |
| `tab_list` | decoded, stored | Header/footer anonymous-NBT components parsed and queued for future tab-list UI. |
| `tag_query` | decoded, stored | Transaction id and optional NBT parsed and queued; no query UI consumes it. |
| `take_item_entity` | decoded, dispatched, stored | Item/player/amount parsed; world keeps the last 64 pickup-animation events. |
| `ticking_state` | decoded, dispatched, stored | Tick rate/frozen state retained; frozen mode suspends simulation until a step grant. Non-default tick-rate pacing is not applied. |
| `ticking_step` | decoded, dispatched, stored | Nonnegative step count parsed; simulation consumes one granted tick at a time while frozen. |
| `update_attributes` | decoded, dispatched, stored | Attribute bases/modifiers parsed and stored by entity id, with entity-map bounds. |
| `update_recipes` | deferred | Recipe-book update variants are not decoded. |
| `update_tags` | deferred | Registry/tag synchronization payload is not decoded. |
| `set_projectile_power` | decoded, stored | Entity id and acceleration parsed and queued for a future projectile system. |
| `custom_report_details` | decoded, stored | Bounded key/value detail list parsed and queued; no report UI consumes it. |
| `server_links` | decoded, stored | Built-in/custom labels and URLs parsed and queued for future server-info UI. |
| `reset_chat` | decoded, dispatched | Configuration-phase packet clears the in-memory chat log. |

## Validation evidence

- `python3 tools/v767_total_parity_audit.py`: Play/Clientbound 115/124 covered.
- `cargo check --offline` from `client-app`: passed.
- `cargo test --release --offline -j2 --test v1211_proto767_tests --test world_entities_ber_tests` from `client-app`: passed, 17 + 8 tests.
- Requested full `cargo test --release --offline -j2` from `client-app`: 81 library tests passed, one unrelated existing test failed at `src/integrated.rs:551`; it selected `/home/yuu/mc-spine` as runtime root instead of its temporary fixture. Re-running that test with `/tmp` as the command working directory produced the same failure because the executable's ancestors include the repository root.

The full-test failure is environment/path discovery evidence, not evidence that
the packet changes pass the entire suite. No live Minecraft server/client
session was run.
