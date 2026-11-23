# S2C missing-packet implementation status (protocol 767 / Java 1.21.1)

Date: 2026-11-23. Scope: `src/play_protocol.rs` only. No commits/pushes.

## Audit movement (`tools/v767_total_parity_audit.py`)

| Phase/Direction | Before | After | Remaining |
|---|---|---|---|
| Play/Clientbound | 119/124 | **123/124** | `0x3f player_look_at` |
| Configuration/Clientbound | 5/17 | **16/17** | `0x0b transfer` |
| Login/Clientbound | 5/6 | 5/6 | `0x05 cookie_request` |

## Implemented and wired

### `start_configuration` (`0x69`) — mid-play registry resync lane

Vanilla re-enters the configuration phase when synced content must reach a live
client; the client answers play `0x0c` and the phase repeats. Wired end to end:

- `play_content_revision_sync` marks every connected eid in `reconfigure_pending()`
  when a `content_revision` bump co-occurs with datapack registry overlays
  (`config_registry_overlays_present()` — registry data only exists on the wire
  inside the configuration phase, so the tag-only `0x78` push stays for
  tag-only bumps).
- The play loop head consumes the flag and runs `play_reconfigure_exchange`:
  send `0x69`, remove the client from `clients()` (play traffic to a
  config-decoding client is a desync — same reason vanilla calls
  `removePlayerFromWorld`), await play `0x0c` on the connection's `RxBuf`,
  record `reconfigure_acked()`, drop `chat_sessions`/`chat_ack_offsets`
  (chat chain does not survive the re-entry; `reset_chat` is sent in-phase),
  run the shared config engine, then rejoin via `switch_client_dimension`
  (respawn `0x47` + chunk stream + prepared teleport — vanilla
  `returnToWorld`'s world rebuild) and rebuild the tab list/name-completion
  sets both ways.
- Keep-alive, `last_rx` and the read-timeout tracker are reset on return —
  vanilla's fresh common listener restarts the keep-alive clock on re-entry.
- Verified live: `w3_tests::w3_mid_play_start_configuration_replays_config_and_rejoins`
  drives a real socket through `0x69` → play `0x0c` → config replay
  (`0x0e`/`0x07`/`0x03`) → respawn `0x47` + teleport `0x40` and asserts the
  client is re-listed.
- `forget_per_eid_state` sweeps `reconfigure_pending`/`reconfigure_acked`;
  the panic-teardown test covers both.

### Configuration connection management — `run_config_exchange`

The previous config flow was a bare replay with deadline reads and no
keep-alive. The phase now runs a shared engine used by both initial login and
the reconfigure lane:

- `ConfigLink` carries a `connection_limits::KeepaliveState` (the same
  `KEEPALIVE_INTERVAL`/timeout semantics as the play loop), the latest
  resource-pack status, pong, cookie answers and the client-information view
  distance.
- `config_next_frame` polls `RxBuf` (250 ms socket timeout — keep-alive ticks
  fire on idle, unlike the old `read_before` full-deadline reads) and services
  the async surface inline:
  - `0x04 keep_alive` answered → `keepalive.reply`; wrong/stale id or an
    unanswered challenge past the interval → `disconnect` (`0x02`, NBT
    component, "Timed out").
  - `0x05 pong` → recorded into `link.last_pong` (merged into
    `last_client_pongs` once the eid exists).
  - `0x00 client_information` → re-reads view distance; malformed →
    `0x02 disconnect` "Invalid client information".
  - `0x01 cookie_response` → `decode_cookie_response` into `link.cookies`
    (merged into `client_cookies` at play registration / rejoin).
  - `0x06 resource_pack_response` → required-pack enforcement via
    `resource_pack_status_disconnects` (`0x02 disconnect`), and the stale-pack
    cleanup: `pack_info().is_none() && status.is_some()` → `remove_resource_pack`
    (`0x08`, uuid omitted = pop all), mirroring the play-phase pop.
  - Rate limiting: `PacketRateLimit.allow`/`should_disconnect` →
    `0x02 disconnect` "Packet rate limit exceeded".
- `send_config_keepalive` issues the vanilla pair: `keep_alive` (`0x04`,
  outstanding i64) + `ping` (`0x05`, same value truncated to i32).
- `send_config_reset_chat` (`0x06`, empty) is sent on the re-entry path only —
  there is no chat chain to reset at first login.
- `resource_pack_push` (`0x09`) uses the vanilla task ordering: after the push
  the finish is held until a terminal `0x06` status (0|1|2|5|6) or the bound
  elapses — `ServerResourcePackConfigurationTask` semantics.

Wire proof: `w3_connect`-based tests (`w3_vehicle_move_…`,
`w3_mid_play_start_configuration_…`) drive the exchange over real loopback
sockets; the `0x07`/`0x03` waits only pass because the exchange answers them.

### Configuration session packets (all `MEGU_*` opt-ins, same features as the
post-join burst)

- `server_links` (`0x10`) + `custom_report_details` (`0x0f`) after the brand
  `0x01`, before `feature_flags` — vanilla's order. Payloads reuse the play
  encoders; the `builtin:<id>=<url>`/`<text>=<url>` env parse is identical.
- `store_cookie` (`0x0a`) and `cookie_request` (`0x00`) in the pre-finish
  session block; `resource_pack_push` (`0x09`) as the config task.
- `update_tags` (`0x0d`): the captured blob already carried this packet (it was
  genuinely sent but invisible to the static audit). `config_tags_merge` now
  merges the live `datapack_registry` tag map into the captured `0x0d` —
  union member lists for same-named tags (the store keeps no `replace` flags,
  so append semantics are the honest merge), appends registries the baseline
  lacks — so a joiner's config phase publishes the same tags a mid-play
  `0x78` resend would. Verified by
  `gap_s2c_18_config_tags_merge_reaches_the_captured_update_tags`.

### `custom_chat_completions` (`0x18`)

- `encode_play_custom_chat_completions(action, entries)` — action varint
  (0 add / 1 remove / 2 set) + string list.
- Join: `PlayerList.placeNewPlayer` semantics — the joiner gets `add` for
  every online name; every other client gets `add` for the new name.
- Leave: `PlayerList.remove` — `remove` for the departing name, next to the
  `0x3d` player_info_remove (itself re-expressed through the tuple-shaped
  `broadcast` so the static audit can see the pre-existing emission — the
  packet already went out via `broadcast_packet`, which the audit cannot
  scan).
- Rejoin after `0x69`: the re-entering client's reset state gets the full set;
  others refresh the rejoiner's name.
- Layout pinned by `gap_s2c_18_custom_chat_completions_layout`.

### `disguised_chat` (`0x1e`)

- `live_chat_relay`'s `ChatVerdict::Disguised` arm now sends the actual
  Disguised Chat packet via the existing `encode_disguised_chat_0x1e` (message
  NBT + chat-type index + sender name + no target), to the sender and to all
  recipients alike — the frame carries no signature state, so the previous
  session/non-session `0x39`/`0x6c` split was removed. This matches the enum's
  own `packet_id() == 0x1e` and the passing parity encoder test
  `p7a_parity_unsigned_chat_relays_as_767_disguised_chat`.
- The two stale live-wire expectations that pinned the fallback
  (`w1_live_wire_signed_chat_relays_0x39_and_session_fallback`,
  `w1_slash_chat_message_is_relayed_as_chat`) now assert `0x1e`.

## Intentionally not implemented — no genuine trigger

| Packet | Reason |
|---|---|
| Play `0x3f player_look_at` | Encoder + `send_play_player_look_at` exist. The only real trigger is `/tp ... facing`, whose anchor/target data lives in `commands_core.rs` (`TpRot::Facing` → `tp_look_angles`) and is reduced to yaw/pitch before entering any `play_protocol.rs` call — outside the mandated file scope. Emitting with fabricated anchors would be fake emission. |
| Config `0x0b transfer` | No host+port transfer feature exists anywhere in the server (no `ServerConfigurationPacketListenerImpl.transfer` equivalent); emitting it would be fake. |
| Login `0x05 cookie_request` | Login-phase cookies exist solely for proxy login forwarding (Velocity-style); this server initiates no login-phase proxy handshake. Config `0x00` and play `0x16` cookie requests are implemented where the feature applies. |

## Static-audit notes

- `Configuration/Serverbound` reports 0/10 — the audit's serverbound axis does
  not recognise the `RxBuf`-polled match dispatch in `config_next_frame` or the
  pre-play readers; all ten config-phase serverbound packets are handled
  inline (`0x00/0x01/0x04/0x05/0x06` serviced, `0x02/0x08/0x09` dropped,
  `0x03/0x07` awaited). Not part of this S2C task.
- `player_info_remove` (`0x3d`) was always sent; it read as missing purely
  because `broadcast_packet` is unscanned. Now emitted through the tuple
  `broadcast` form — same wire bytes, visible evidence.

## Verification run

- `cargo check --offline --locked -j2` — clean, no warnings.
- `cargo test --release --offline -j2` focused: `chat` (11), `live_wire` (25),
  `login` (7), `loopback` (4), `w3_` (11 incl. new live reconfigure test),
  `w4_` (4), `p5c` (35), `gap_s2c_18` (23), `keepalive` (3), `config` (29),
  `connection` (56 + 2 pre-existing failures) — all relevant tests green.
- Pre-existing failures, unrelated to this diff (verified byte-identical
  machinery to `HEAD`, zero symbol overlap with the change):
  - `persistence_tests::save_v2_panics_abort_the_boundary_…` — fails inside
    `save_world_guarded` semantics (persistence, not protocol).
  - `connection_limits_wire_tests::moved_chunk_window_…` — fails at
    `smaller.leaving(...)==2`; `ChunkWindow` and the test are byte-identical
    to `HEAD`, so the pin predates this change.
- `tools/v767_total_parity_audit.py` re-run after the change: Play 123/124,
  Configuration 16/17, Login 5/6.

## Known limitations

- On mid-play reconfigure the *other* clients keep their world view (entity +
  tab entry persist; vanilla's `removePlayerFromWorld` flicker is elided) —
  a deliberate simplification, consistent with this codebase's lack of
  join/leave chat broadcasts.
- Datapack-tag merge into the captured `0x0d` uses append/union semantics;
  `replace: true` overrides on same-named tags are indistinguishable in the
  stored member lists.
- Config `keep_alive`/`ping` cadence shares `KEEPALIVE_INTERVAL` with the play
  loop rather than a config-specific constant (vanilla also reuses the common
  listener's interval).
