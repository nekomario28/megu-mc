# Pre-Release Security, Exploit & Wiring Audit — 2026-11-22

Lane: `REL-SECURITY-EXPLOIT-AUDIT-1`. Scope: entire workspace (root `mc-spine`
crate + 12 library crates), adversarial review of the live network/play path,
mod-sandbox file/network handling, and end-to-end wiring.

Audit basis: `tools/wiring_audit.py` and `tools/p0_2_audit.py` (read-only),
plus manual review of `proto/src/wire.rs`, `src/play_protocol.rs`,
`src/main.rs`, `src/connection_limits.rs`, `src/ingress_guard.rs`,
`src/anticheat.rs`, `src/lod_protocol.rs`, `src/commands_core.rs`,
`src/commands_execute.rs`, `src/server_ops.rs`, `src/server_reliability.rs`,
`mod-adapter-core/src/{mod_sync,dep_resolver,modpack,unified_bundle,bytecode_subset_vm}.rs`,
`state-store/src/world_snapshot.rs`, `src/anvil_io.rs`.

---

## 1. Executive Release-Readiness Verdict

**Verdict: BLOCKED for any public/non-loopback deployment in the default
configuration; conditionally shippable for loopback/LAN or behind an
authenticated proxy (Velocity) or with `online-mode = true`.**

The wire codec (`proto/src/wire.rs`), frame/decompression bounds
(`src/connection_limits.rs`), mod-download URL policy (`mod_sync.rs`),
archive path jail (`modpack.rs`), snapshot path jail
(`state-store/src/world_snapshot.rs`), command permission table
(`server_ops.rs`), RCON throttle, and the bytecode-subset VM fuel cap are all
in good shape — this codebase has clearly been through several hardening
passes. The residual risk concentrates in three places:

1. **Identity is name-only in the default (offline) mode**, and every
   privilege decision — ops level, whitelist, bans, `/megu`, OP-gated C2S
   packets — keys off that unverified name. One forged Login Start is a
   level-4 compromise. (C1)
2. **A panic inside a connection handler leaves a permanent ghost client**:
   `handle()` runs on a bare `thread::spawn` with no `catch_unwind`, and the
   disconnect cleanup (client-list removal, inventory save, menu close,
   per-eid map cleanup) only runs on the normal break path. Several
   attacker-reachable panic sources exist (unchecked map index, poisoned
   mutex `.lock().unwrap()`). (H2)
3. **The opt-in `mc-spine:lod_v1` plugin channel is an uncached,
   unrate-limited worldgen/CPU amplifier** reachable by any client that
   registers one string on `minecraft:register`. (H1)

Everything else is MEDIUM/LOW hardening or dead-code/wiring gaps.

---

## 2. Findings

| # | Severity | Location | Finding |
|---|----------|----------|---------|
| C1 | CRITICAL | `src/play_protocol.rs:6557-6572`, `src/commands_core.rs:5483-5501`, `src/server_ops.rs:117-126`, `src/server_config.rs:632,806` | Offline-mode name-only identity → op/whitelist/ban impersonation → level-4 command execution |
| H1 | HIGH | `src/lod_protocol.rs:282-310`, `src/lod_protocol.rs:206-239`, `src/worldgen.rs:9882-9885` | `mc-spine:lod_v1` REQ1 handler: unbounded per-packet worldgen/CPU cost, no cache, no rate limit, 128-chunk radius |
| H2 | HIGH | `src/main.rs:16787-16791`, `src/play_protocol.rs:8882`, `src/play_protocol.rs:8835,8844,9898,10876,7367,12575` | No `catch_unwind` around `handle()`; panic skips all disconnect cleanup → permanent ghost client; several attacker-reachable panic sources |
| M1 | MEDIUM | `src/ingress_guard.rs:14-27`, `src/play_protocol.rs:8651-8656` | No per-packet rate limit for gameplay packets; every accepted move broadcasts to all trackers — egress/CPU amplification |
| M2 | MEDIUM | `src/play_protocol.rs:1300-1315`, `src/server_reliability.rs:513` | Mojang session/skin lookups `read_to_string()` with no response-size cap |
| M3 | MEDIUM | `src/play_protocol.rs:7450-7452` | `chat_message` (0x06) text starting with `/` executes commands — vanilla divergence; permission still enforced but chat_command packet semantics are bypassed |
| M4 | LOW | `src/connection_limits.rs:92-124`, `src/play_protocol.rs:522-534` | Decompressed frame bound is operator-widenable to 32 MiB vs 2 MiB wire cap (bounded, but a ~16x zip-bomb ratio per connection) |
| M5 | LOW | `src/play_protocol.rs:1289-1316` | Skin lookup keyed by lowercased name; rejoin/name churn forces outbound Mojang HTTPS calls (capped at 4 concurrent + LRU cache, but no negative-result TTL beyond cache) |
| W1 | LOW (release note) | `tools/wiring_audit.py` output | 26 production modules have no consumers; 1 wiring contract pending (`state-store/world_snapshot`) — dead code ships untested |
| W2 | LOW | `tools/p0_2_audit.py` output | Spawn-reachability gaps: 146 unreachable weighted spawn uses (enderman/bat/axolotl/water mobs) — gameplay completeness, not security |

---

## 3. Finding Details

### C1 — CRITICAL: offline-mode name claim = operator impersonation

**Mechanism.** In the default configuration (`online-mode = false`,
`src/server_config.rs:632`), `login_identity`
(`src/play_protocol.rs:6557-6572`) accepts any `[A-Za-z0-9_]{1,16}` name and
derives the UUID as `md5("OfflinePlayer:" + name)` — no Mojang session check.
Online mode exists (`online_login`, `src/play_protocol.rs:6924-6965`) but is
opt-in via `MC_SPINE_ONLINE_MODE` / `online-mode` (`server_config.rs:806`,
`play_protocol.rs:6879-6881`), and Velocity forwarding is the alternative gate.

Every privilege decision then keys on the *claimed* name:

- `player_permission_level(name)` → `OpsRegistry::op_level_of(name)`
  (`src/commands_core.rs:5483-5501`, `src/server_ops.rs:117-126`). The
  registry stores a `uuid` field (`server_ops.rs:85`) but `find()` compares
  **only** `e.name.eq_ignore_ascii_case(name)` — the UUID is written on
  `set()` (`server_ops.rs:155`) and parsed on load (`server_ops.rs:196-197`)
  yet never consulted at lookup.
- `check_login_admission` (`src/play_protocol.rs:7895-7903`) enforces
  whitelist/bans the same name-keyed way, so bans and whitelist are equally
  spoofable (a banned player rejoins under any unused name).
- OP-gated C2S packets all call `player_permission_level(&me.name)`:
  `block_entity_tag_query` (`play_protocol.rs:12258`),
  `debug_sample_subscription` (`:12325`), `entity_tag_query` (`:12342`),
  `set_command_block`/`set_command_minecart` (`:12424,12438`),
  `lock_difficulty` (`commands_core.rs` via `c2s_apply_lock_difficulty`,
  `play_protocol.rs:4774`), command dispatch (`run_command_ctx`,
  `commands_core.rs:7671-7684`).

**Exploit.** Operator "Steve" is level 4 in `ops.json`. Attacker connects to a
public bind, sends Login Start with name `Steve`. Server derives Steve's
offline UUID, admits them, and `player_permission_level("Steve") == 4`.
Attacker runs `/stop`, `/publish`, `/megu policy set …`, `/gamemode creative
@a`, `/give @a netherite_block 64`, sets command blocks, etc. Full server
compromise with zero credentials.

**Fix.** For v0.1.0-alpha: (a) refuse non-loopback binds when neither
`online-mode` nor Velocity forwarding is configured (fail closed at startup,
like `connection_limits::check_bind` already does for RCON); (b) when a
player authenticated via online-mode/forwarding, resolve op level by UUID
first and fall back to name only in offline mode; (c) print a loud startup
warning that offline mode trusts claimed names.

### H1 — HIGH: `mc-spine:lod_v1` REQ1 is an uncached worldgen/CPU amplifier

**Mechanism.** `custom_payload_c2s` (`src/lod_protocol.rs:316-338`) accepts
`mc-spine:lod_v1` requests once a client registers the channel
(`lod_v1_register`, `:260-269` — registration is a single unauthenticated
string in `minecraft:register`). `lod_v1_request` (`:282-310`) validates only
magic, resolution, and a Chebyshev range of `LOD_V1_MAX_RANGE_CHUNKS = 128`
(`:37`) — then calls `encode_lod_v1_column` → `lod_column_cells` (`:206-239`),
which runs `lod_top_cell` (`:183-201`) for **all 256 block columns** of the
chunk. Each `lod_top_cell` calls `height_at_dim` (`src/main.rs:6606-6620`)
—which for worldgen dimensions calls `worldgen::chunk_cols_dim`
(`src/worldgen.rs:9882-9885`), i.e. **on-demand chunk generation** — then
scans `y` downward calling `block_at_dim` (`src/main.rs:8977-8991`), each of
which takes the world lock (`timed_world()`). Worst case per REQ1: 256
heightmap queries (each possibly generating a chunk) + up to ~256×(surface+32
− min_y) block lookups ≈ tens of thousands of world-lock acquisitions, all on
the connection thread, with **no result cache and no rate limit**.

**Exploit.** A client registers `mc-spine:lod_v1`, then floods REQ1 frames
for distinct `(cx,cz)` inside the 128-chunk radius (~66k distinct columns,
vs ~400 a vanilla view-distance-10 client ever requests). Each packet forces
chunk generation far outside any player's view and burns a core; the attack
surface is a custom channel vanilla servers don't have, so there is no
vanilla-equivalent mitigation to point at. Requests within the same column
are also recomputed per packet — spam one column = pure CPU burn.

**Fix.** Cache `LOD1` frames per `(dim, cx, cz, resolution)` (invalidate on
block edits like the chunk packet cache does), rate-limit REQ1 per
connection (e.g. token bucket), consider shrinking the radius toward the
client's view distance, and/or move encoding off the connection thread.

### H2 — HIGH: panic in `handle()` leaves a permanent ghost client

**Mechanism.** Connections run on `thread::spawn(move || { … handle(s) })`
with **no `catch_unwind`** (`src/main.rs:16787-16791`). The entire disconnect
cleanup — `clients().retain`, per-eid map removal, inventory save
(`inv_store`/`pos_store`/`save_live_player_snapshot`), chest-window close,
mount/lead detach — sits at the end of `handle`'s normal flow
(`src/play_protocol.rs:12485-12639`) and only runs when the `'conn` loop
breaks cleanly. Any panic unwinds past all of it: the `Arc<Client>` stays in
`clients()` forever (counted in `max_players`, still rendered/tracked by
other players), the inventory is never persisted (silent item loss), open
container windows keep stale opener counts, and every per-eid map entry
(`death_drop_marks`, `chat_sessions`, `client_cookies`,
`client_abilities_flying`, `seen_advancement_state`, `combat_sessions`,
`reconfigure_acked`, `beacon_menu_anchors`, `player_mechanism_sessions`,
`spectator_cameras`) leaks for the process lifetime.

Attacker-reachable panic sources found:

- `src/play_protocol.rs:8882` — `inv[&asl]` indexes the inventory map with no
  presence check. `asl` was found at `:8841-8847`, but `me.inv` is a shared
  `Mutex` that RCON/console commands (`/clear`, `/item replace`, `/kill`)
  mutate on other threads; if the arrow stack vanishes in the window, the
  index panics **while the `MutexGuard` is held**, poisoning `me.inv`.
- Bare `.lock().unwrap()` on mutexes inside packet handling — panic the
  instant the mutex is poisoned: `play_protocol.rs:8835` (`me.bow_draw`),
  `:8844` (`me.inv`), `:9898` (`clients()` — **global** mutex: a panic here
  kills the packet for every connection that touches `clients()` while
  poisoned), `:10876` (`signs()` — global), `:7367` (`live_admission()` —
  global, hit on every chat message), `:12575` (`inv_store()` — global,
  hit on every logout), `:1762`, `:2746`, `:5398`, `:7247`.
- The poisoned-mutex cascade is self-reinforcing: once e.g. `me.inv` is
  poisoned by the `inv[&asl]` panic, every subsequent bow release hits
  `.lock().unwrap()` at `:8835`/`:8844` and panics again — the connection
  can never process another packet cleanly, yet the ghost client persists.

**Exploit.** Join, draw a bow (0x24 start-dig on a bow), then have an
operator/RCON run `/clear <name> arrow` (or any inventory mutation) in the
microsecond window before the release packet is handled — or more simply,
rely on any latent panic bug elsewhere; each panic permanently leaks a
client slot and ghost entity. Repeated panics exhaust `max_players` with
ghosts and leak memory.

**Fix.** Wrap `handle(s)` in `catch_unwind` and run the same cleanup block on
panic (or restructure cleanup into a `Drop` guard on the connection state);
replace every `.lock().unwrap()` on shared/player mutexes with
`unwrap_or_else(|e| e.into_inner())` (the pattern already used everywhere
else in the file); change `inv[&asl]` to a checked `get`/`remove`.

### M1 — MEDIUM: no gameplay-packet rate limit; movement broadcast amplification

`src/ingress_guard.rs:14-27` documents the deliberate absence of a
per-packet rate limit ("vanilla has none"). Each accepted `0x1a`/`0x1b`
movement packet still does real work: `ac_check_move`
(`src/anticheat.rs:1428-1517` — block scans around the player),
`ChunkWindow::at` + `pump_client_chunks` on window change
(`play_protocol.rs:8639-8650`), and `broadcast_seen` → `entity_teleport` to
every tracking client (`:8651-8656`). A hacked client streaming position
packets at line rate multiplies per-packet cost by the viewer count and can
pin cores; the anti-cheat only *flags after a threshold*
(`anticheat.rs:1491-1493`) and accepts the move regardless until then.
Recommendation (not a vanilla-parity violation to fix silently): per-
connection token bucket on non-movement packets at minimum, and consider
coalescing movement to one evaluation per tick like vanilla's
`ServerGamePacketListenerImpl` tick processing.

### M2 — MEDIUM: unbounded Mojang response reads

`load_skin_properties` (`src/play_protocol.rs:1289-1316`) and
`mojang_http_get` (`src/server_reliability.rs:501-520`) call
`body_mut().read_to_string()` with no size cap on the response. The endpoints
are fixed `https://api.mojang.com` / `sessionserver.mojang.com` over TLS, so
exploitation requires a compromised/MITM'd Mojang endpoint — low likelihood,
but a hostile or buggy upstream could OOM the server. Cap the read (e.g.
`take(1 << 20)`).

### M3 — MEDIUM: `chat_message` executes `/`-commands

`live_chat_message` (`src/play_protocol.rs:7450-7452`) runs
`run_command(Some(me), &m, …)` whenever the chat *text* starts with `/`.
Vanilla only executes commands from the `chat_command` packet (0x04/0x05);
`chat_message` (0x06) never runs commands. Permission is still enforced by
`run_command_ctx`, so this is not a privilege bypass, but it means a client
can smuggle command text through the signed-chat path (which has different
signature/admission semantics — `live_chat_validate` is skipped for the
`/` early-return at `:7450-7453`, before `live_chat_validate` runs at
`:7466`). Verify whether the early return intentionally precedes signature
validation; if so, unsigned/disguised chat can trigger commands that the
signed-chat admission path would have rejected. Recommend moving the `/`
check after `live_chat_validate` or restricting command execution to the
chat_command packet only.

### M4 — LOW: operator-widenable decompression bound

`decode_compressed_frame` (`src/connection_limits.rs:92-124`) allocates
`vec![0; length + 1]` where `length` is the attacker-declared uncompressed
size, capped by `live_play_frame_bound()` (`play_protocol.rs:522-534`) —
default 2 MiB (`max_frame_mib = 2`), operator-settable to 32 MiB. The wire
frame itself stays ≤ `MAX_PLAY_FRAME` (2 MiB − 1,
`ingress_guard.rs:30`), so worst case is a ~16× expansion per connection.
Bounded and deliberate, but worth a per-IP aggregate memory note in docs.

### M5 — LOW: skin-lookup fan-out

`kick_skin`/`load_skin_properties` (`play_protocol.rs:1241-1364`) issues up
to two outbound HTTPS calls per uncached name, capped at
`SKIN_LOOKUPS_MAX = 4` concurrent (`:1243`) with an LRU cache
(`skin_cache_get`, `:1352`). A join/part flood with rotating valid names
generates sustained Mojang API traffic from the server IP (Mojang may
rate-limit the host). Negative results are cached (`skin_cache_insert`),
mitigating repeats; consider a longer negative TTL or a per-IP join-rate
limit (see M1).

### W1 — Wiring gaps (26 unwired modules + 1 pending contract)

`python3 tools/wiring_audit.py` → **221/247 wired, 26 unwired; 16/17
contracts wired, 1 pending**:

- **Pending contract:** `state-store/world_snapshot` → `src/anvil_io.rs` or
  `src/main.rs`. The world backup/restore engine
  (`state-store/src/world_snapshot.rs`, incl. the `validate_rel_path` jail at
  `:535-564` and atomic staging swap at `:1291-1369`) has **no production
  call site** — the feature is dead code in v0.1.0-alpha.
- Unwired (no production consumers): `core/src/math_opt_foundation.rs`,
  `core/src/server_perf_governors.rs`, `proto/src/resolver.rs`,
  `proto/src/session.rs`, `proto/src/voice_relay.rs`,
  `state-store/src/{anvil_inspect,audit_log,pregen,region_tick}.rs`,
  `server-files/src/{claims_wire,directory}.rs`,
  `mod-offload/src/{combat_motion,entity_spatial,ffi,me_network,party_storage}.rs`,
  `mod-adapter-core/src/{custom_media,dep_resolver,mine_cut_all}.rs`,
  `create-adapter/src/orbital.rs`, `client-port/src/{atlas,upload_budget}.rs`,
  `client-kernel/src/{command,ffi,occlusion_table}.rs`.
- Notable consequence: `dep_resolver.rs` (Modrinth artifact resolution with
  sha256/sha512 verification, `:540-560`, `:1464-1482`) is **unwired** — the
  only live mod-install path is `live_mod_bridge`'s operator-armed
  `mod_sync` manifest (`src/live_mod_bridge.rs:732-788`), which serves
  manifests to clients but does not itself download/install JARs.
  `mod-offload`'s `ffi.rs` and `client-kernel`'s `ffi.rs` export `extern "C"`
  symbols into `cdylib`s that nothing loads — dead FFI surface, not
  exploitable today but must not ship silently.

### W2 — `p0_2_audit.py` datapawn reachability failures

`python3 tools/p0_2_audit.py` reports **12 failure groups**: ocelot
mis-categorized under Monster spawners in `bamboo_jungle`/`jungle`
(registry category `Creature` vs biome `Monster`), and 146 unreachable
weighted spawn uses — enderman unreachable in all 5 End biomes, bat/axolotl/
glow_squid/cod/pufferfish/salmon/tropical_fish/dolphin/squid unreachable in
the listed overworld biomes. Gameplay-completeness gaps, not security; list
as known-parity-debt for alpha.

---

## 4. Wiring & Feature Completeness Matrix (13 crates + root)

| Crate | Wired? | Production consumers | Notes |
|-------|--------|----------------------|-------|
| `mc-spine` (root, `src/`) | ✅ live server | `main.rs` listener, RCON, console | 18k-line `play_protocol.rs` is the real protocol impl; `include!`s ~30 modules |
| `mc-spine-proto` (`proto/`) | ⚠️ partial | `client-app` uses codec/wire/table/data_v767/777/friend_relay/version | `resolver.rs`, `session.rs`, `voice_relay.rs` unwired; server does NOT use the crate's codec for play (duplicated impl in `play_protocol.rs`) |
| `mc-spine-core` (`core/`) | ⚠️ partial | `sys_metrics`, `hw_adaptive_engine`, `perf_kit`, etc. via feature `mod-suite`/client crates | `math_opt_foundation.rs`, `server_perf_governors.rs` unwired |
| `mc-spine-state-store` | ⚠️ partial | `sharded`, `snapshot`, `incremental_save`, `migrate`, `datafix` used by `anvil_io`/`save_v2` | `world_snapshot` (backup/restore), `anvil_inspect`, `audit_log`, `pregen`, `region_tick` unwired — pending contract |
| `mc-spine-server-files` | ⚠️ partial | `properties`, `eula`, `lists`, `usercache`, `atomic`, `json`, `perf_cache` | `claims_wire.rs`, `directory.rs` unwired — claims/directory features dead |
| `mod-adapter-core` | ⚠️ feature-gated | `live_mod_bridge` (`mod-suite` feature) uses `mod_sync`, `unified_bundle`, `magic_primitives`, `modpack` (zip reader), `auto_adapter`, `class_mixin_scanner`, `ecosystem_compat`, `loader_extensions` | `dep_resolver`, `custom_media`, `mine_cut_all` unwired; only active with `--features mod-suite` |
| `create-adapter` | ⚠️ feature-gated | `mod_offload_bridge`/`live_mod_bridge` under `create-offload`/`mod-suite` | `orbital.rs` unwired |
| `mod-offload` | ⚠️ feature-gated | `mod_offload_bridge` under `create-offload` | `ffi.rs` (cdylib `extern "C"`), `combat_motion`, `entity_spatial`, `me_network`, `party_storage` unwired |
| `mc-spine-kernel` (`client-kernel`) | ⚠️ partial | `client-app`/`client-port` pipeline | `command.rs`, `ffi.rs` (cdylib), `occlusion_table.rs` unwired |
| `mc-spine-client-mesh` (`client-port`) | ⚠️ partial | `client-app`/`client-gpu` render path | `atlas.rs`, `upload_budget.rs` unwired |
| `mc-spine-client` (`client-app`) | ✅ client binary | `cli`, `session`, `protocol`, `render`, `sim`, `world`, `lod`, `friend_client`, `recovery`, `crash_reporter` | uses `mc-spine-proto` codec |
| `mc-spine-client-ui` (`client-ui`) | ✅ | contracts: `state.rs`, `layout.rs` cover `mod_menu`, `recipe_browser`, `magic_hud` | all contracts wired |
| `mc-spine-client-gpu` (`client-gpu`) | ✅ | contracts: `renderer.rs`, `live_session.rs` cover `atmosphere`, `subworld_render`, `space_sky_bridge`, `builtin_shadow_wgsl`, `vanilla_plus_*` | all contracts wired |
| `compat/` | n/a | Java-compat fixtures (`fabric-probe`, `neoforge-events` features) | not a crate; JNI-gated |

Feature gates worth noting for release: `mod-suite`, `create-offload`,
`neoforge-events`, `fabric-probe`, `java-compat`, `native-oracle` are all
opt-in; the default build is the vanilla-parity server + client-app. The
mod-sandbox audit surface (`mod_sync`, `modpack`, `dep_resolver`,
`bytecode_subset_vm`, `unified_bundle`) is therefore **largely dormant in a
default build** — the findings above on those files apply when the features
are enabled.

---

## 5. Areas Verified Clean (adversarial pass, no findings)

- **Wire codec** (`proto/src/wire.rs`): VarInt/VarLong strict bounds, UTF-16
  char caps, NBT depth/size caps, anonymous-NBT skip bounds, frame length
  exact-match — fail closed throughout.
- **Live frame path** (`play_protocol.rs:475-630`, `ingress_guard.rs`):
  3-byte VarInt length prefix ≤ 2 MiB − 1; pre-play frames ≤ 64 KiB with a
  single wall-clock deadline (`read_before`, `:460-474`); compressed frames
  bounded on both declared and actual output (`connection_limits.rs:92-124`).
- **Movement**: NaN/Inf and world-border rejection (`ingress_guard.rs:41-66`),
  teleport-confirm gating (`movement_allowed`, `connection_limits.rs:1006`),
  anti-cheat envelope (`anticheat.rs:1428-1517`), vehicle move exact-32-byte +
  finiteness + server-side step validation (`workstations_and_mounts.rs:1805-1818`,
  `play_protocol.rs:7195-7232`).
- **Gamemode gating**: `set_creative_slot` requires `gm == 1`
  (`play_protocol.rs:10885`), `FillCreative` pick plan gated on creative
  (`:2269-2330`), adventure `may_destroy` (`:9184-9189`), OP+creative gates on
  jigsaw/structure/command-block packets (`:12352-12470`), spectator-only
  `teleport_to_entity` (`:12476`).
- **Chat**: 256-char + illegal-char rule (`connection_limits.rs:1310-1314`),
  vanilla spam counter (`:1320-1347`), signed-envelope admission
  (`play_protocol.rs:7360-7371`).
- **RCON**: env-only enable, nonempty password required, loopback default
  bind, `check_bind` public-bind guard, per-IP `AuthThrottle` with lockout,
  constant-time compare (`connection_limits.rs:1353-1360`), 8-connection cap,
  8 KiB frame cap (`main.rs:14238-14369`).
- **Mod downloads** (`mod_sync.rs`): HTTPS-only, no userinfo, no IP
  literals, port-443 only, no local/internal hosts, exact-match trusted CDN
  allowlist (`:560-640`); `.jar` basename jail (`:648-693`); atomic staged
  install with symlink refusal and post-install re-hash (`:1432-1501`);
  prune validates every name before deleting (`:1505-1549`).
- **Archive handling** (`modpack.rs`): `validate_member_path` rejects `..`,
  absolute, drive-letter, backslash, empty segments, NUL (`:189-217`);
  bounded member extraction (`:1433-1511`).
- **Snapshot restore** (`world_snapshot.rs`): `validate_rel_path` jail on
  every manifest path (`:535-564`), per-object sha256 re-verify
  (`:1057-1101`), symlink refusal, staged-then-swap restore (`:1291-1369`).
  *(Currently unwired — W1.)*
- **Bytecode VM** (`bytecode_subset_vm.rs`): verified-before-execute,
  host-call allowlist, strict instruction fuel → `OutOfFuel` on infinite
  loops (`:36-48`, `:1752-1775`).
- **Dep resolver** (`dep_resolver.rs`): sha256-preferred constant-time digest
  verify, sha512 fallback, sha1 never sufficient (`:540-560`); consent token
  required for install (`:1464-1482`). *(Unwired — W1.)*
- **Per-eid cleanup**: disconnect path removes all per-connection maps
  (`play_protocol.rs:12603-12629`) — when it runs (see H2).
