# mc-spine security notes

## FIXED 2026-09-25 (root cause confirmed) — negative VarInt write_varint runaway

The production OOM (67GB VmSize / ~20GB RSS, ~33-100s after a client
connects, 3+ kernel kills) was **NOT** the packet-length path — it was
`write_varint()` encoding a negative `i64` with arithmetic `>>=`:
`v` never reaches 0 → pushes `0xFF` into the packet Vec forever → one
anonymous mapping doubles 1→2→…→64GiB. Reproduced live: buggy build +
one move packet to x=-100 → 682MB in <0.5s → cgroup-killed at 4GB cap.

Fire paths (all funnel through the same `write_varint`):
- `stream_chunks` writes `ccx`/`ccz` — **normal play at x<0 or z<0**
- `ack(seq)` echoes raw wire seq (dig/place/use_item) — seq<0 client
- `set_creative_slot` cnt/item — raw wire i64s (also poisons `me.inv`
  → every later slot echo re-loops)
- sheep-graze packed position when bx<0

Fix in source: `v < 0 && v >= i32::MIN → v as u32 as u64` (5-byte
VarInt), wider negatives → `v as u64` (10-byte VarLong). Regression
probes: `probe_negpos.py` (negative-coordinate move) and
`probe_neg_varint.py` (use_item seq=-1, 10-byte LEB128) — both flat on
fixed builds, runaway on the buggy one.

## FIXED 2026-09-25 — unbounded packet-length allocation (DoS / OOM)

`read_packet` (src/main.rs ~line 45) read a VarInt length straight from the
wire and allocated `vec![0u8; len as usize]` with no bound. Up to 5 VarInt
bytes ≈ 34 GiB declared; any client (or a desynced frame) could force a
multi-GB allocation on the unauthenticated listener `0.0.0.0:25599`.

Measured: two production OOM events — 06:51 JST (pid 759851, anon RSS
21.7 GB) and 14:42 JST (pid 850323, anon RSS 20.5 GB), each killing the
server plus host processes via kernel OOM-killer.
(NOTE: later shown to be a real hardening fix but NOT the production-OOM
cause — see the write_varint entry above.)

Fix applied 2026-09-25 ~15:18 JST: reject `len < 0 || len > 16 MiB`
before allocating (line 54). Verified: `cargo check` PASS (MemoryMax=4G
scope). Pre-change file kept at `evidence/foundation/audit-snapshots/main.pre-cap.rs`.
Regression probe: `probe_evil_packet.py` — sends a `0x7fffffff` length
VarInt; patched server must reject with flat RSS. NOTE: any already-
running server process still carries the OLD binary until restarted —
restart under `systemd-run --user --scope -p MemoryMax=4G` regardless.

## FIXED 2026-09-25 (evening) — unbounded accumulators + conn flood

Second hardening pass after the 16 MiB frame cap:

- drop entities now despawn after 5 min (vanilla parity) — previously
  unbounded growth over long sessions.
- xp orbs despawn after 5 min (same fix).
- fishing bobbers / ender pearls are destroyed on owner disconnect —
  previously orphaned entities accumulated per reconnect.
- main listener now caps live handler threads at 64 (thread-per-conn
  flood vector); HTTP pack server got read(5s)/write(10s) timeouts —
  a stalling client no longer wedges the single http loop.
- oversized-packet rejection now also shuts down the socket —
  previously a desynced stream kept churning garbage frames.
- MC_SPINE_PORT env selects the bind port (default 25599) so a
  capped observation instance and a dev instance can coexist.
- verified: probe_evil_packet.py rejects a 0x7fffffff frame with
  flat 3 MB RSS (previously ~2 GiB alloc attempt = kernel OOM-kill).

## FIXED 2026-09-26 (~05:55 JST) — execute depth cap + load_world bounds

Applied by Devin (night session) while `src/main.rs` was quiet >1h:

- `run_execute` now has a recursion-depth cap — `EXEC_DEPTH`
  thread-local, limit 16, same pattern as `UPD_DEPTH`/`run_function_depth`.
  `/execute run execute run …` nested ≥16 replies `"execute depth limit"`.
  Verified live: 40-deep nested execute on patched debug build → reply
  `execute depth limit`, server alive, RSS flat (~5.7 MB).
- `load_world` is bounds-safe — the `rv` varint reader stops at EOF and
  caps the continuation shift at 63; every fixed-width slice loop
  (chunks/mobs/inventory/chests/signs/forceload/furnaces/drops) now has a
  `p + N > b.len()` break. Also fixed a real off-by-4 in the XP-orb
  record guard (`p + 30` → `p + 34`; the record is 34 bytes).
  Verified live: truncated (97 B), all-continuation-byte, and junk MET2
  `world_overlay.bin` all load without panic and the server listens.

## FIXED 2026-09-26 (~17:00 JST) — retainer caps + hostile spawn counting bug

Devin applied while user lane active; both verified in the running
binary (`strings` shows `mob cap reached` / `execute depth limit`):

- **Hostile spawn counting bug (real accretion vector)**: the night-spawn
  "cap 12" only counted etypes 124|23 (zombie/creeper) while spawning
  91|100 (skeleton/spider) too — the uncounted types accumulated every
  night with no despawn. Filter now covers all four hostile etypes.
- `MOB_CAP` = 4096 on the breeding and `/summon` push paths
  (load_world restores are file-bounded and stay exempt).
- `DROP_CAP` = 8192 / `XP_CAP` = 4096 enforced inside the existing
  despawn sweeps — oldest evicted with `broadcast_seen_destroy`.
- `HOPPER_CAP` = 65536 on the index insert in `note_block_update`.
- `world()` overlay intentionally unbounded — it IS the save.
- `nbt_str` length clamp (u16 wrap on >64KiB strings → char-boundary
  clamp) — fixed in the same pass.

Runtime note (2026-09-26 17:0x): the user's Devin lane owns the live
instance — game on :25599, RCON on :25597 (port roles are REVERSED from
the earlier Devin-managed instance). Probing the live server is now the
lane's job; external probes to :25597 hit RCON and reset.

## Remaining findings (unfixed, lower severity — audit-3, 2026-09-25)

- `crops/furnaces/chests/inv_store` — world/player-file bounded growth,
  no caps needed beyond the above. Acceptable residual.
- No authentication on the listener — unchanged (see the 2026-10-25 section
  below for the limits that now exist; there is still no online mode).

## Remaining findings (earlier pass)

- `world` overlay grows ~16 B per edited block, unbounded by design
  (it IS the world save). A bot could spam dig/place packets to grow
  it — the cheapest structural mitigation is a per-chunk diff cap or
  accepting that persistence is intended growth.
- One thread per client — now capped at 64 live handlers; excess
  connections are dropped at accept time.
- No authentication on :25599 — any LAN host can drive the above paths.
  Recommend binding behind a firewall. (A per-IP connection cap and
  deadlines were added 2026-10-25, see below; still no connection *rate*
  limit.)
- String reads are length-clamped by `.min(sl.len())` — safe pattern,
  keep it if touched.

## 2026-10-25 — network hardening (branch `hardening/network-limits`)

### Offline mode trusts the client's identity — read this before exposing the port

The server runs in **offline mode**: the name and UUID in the Login Start
packet are whatever the client says they are. There is no Mojang session
check, no encryption (RSA key exchange + AES/CFB8) and no compression, and
**none of that is implemented here**. Anyone who can reach the listener can log
in as any name, including one that already has saved inventory/position (the
stores are keyed by name). Game traffic and RCON are plaintext.

What changed to contain that, without pretending it is authentication:

- **Public bind needs an explicit opt-in.** The game listener, the RCON
  listener and the resource-pack HTTP listener refuse to bind a non-loopback
  address (`MC_SPINE_BIND_ADDR`, `MC_SPINE_RCON_BIND_ADDR`,
  `MC_SPINE_RESOURCE_PACK_BIND_ADDR`) unless `MC_SPINE_ALLOW_PUBLIC_BIND=1`
  is set. The game listener exits with status 2 and a message; RCON and the
  pack server log the reason and do not start. Existing deployments that bind
  `0.0.0.0` must add the flag.
- **Duplicate logins are refused** (same name, compared case-insensitively, or
  same UUID) with a disconnect message. The identity is reserved atomically at
  login and released only after the first session's inventory/position have
  been saved, which closes the "connect twice, load stale state, overwrite
  the real save" race. It does **not** stop someone from logging in as a player
  who is currently offline.
- **Optional allowlist**: `MC_SPINE_ALLOWLIST="Steve,Alex,01234567-89ab-..."`
  (comma/space separated names and/or UUIDs, unset by default). UUID entries
  match the server-derived `OfflinePlayer:<name>` UUID. Update any entries
  recorded from older client-supplied UUIDs. Offline identities remain
  unauthenticated, so use the allowlist together with a firewall.

### Limits and their knobs (defaults are invisible on loopback)

| env | default | effect |
|---|---|---|
| `MC_SPINE_MAX_CONN_PER_IP` | 4 for non-loopback peers; loopback unlimited unless the variable is set (then it applies everywhere); `0` = unlimited | concurrent connections per remote IP, in addition to the global 64 |
| `MC_SPINE_LOGIN_TIMEOUT_SECS` | 15 (was a fixed 30) | one wall-clock budget for handshake + login + configuration |
| `MC_SPINE_IDLE_TIMEOUT_SECS` | 60 | play state: no *complete* packet for this long drops the client (keep-alive challenges have a separate 15 s deadline) |
| `MC_SPINE_RCON_MAX_FAILURES` | 5 | wrong RCON passwords from one IP before lockout |
| `MC_SPINE_RCON_LOCKOUT_SECS` | 30 | first lockout; doubles per further failure, max 15 min; locked-out IPs are dropped at accept |
| `MC_SPINE_BACKUP_INTERVAL_SECS` | 600 | see persistence below |

Chat and chat commands follow vanilla: more than 256 characters, or a `§` /
control character, disconnects the player; vanilla's spam counter (20 per
message, drains 1 per 50 ms, kick above 200: a burst of 10, sustained 1/s)
kicks with "Kicked for spamming!".

Not covered: a per-IP connection *rate* limit, packet-rate limits for
gameplay packets, and any limit on how fast an unauthenticated client may edit
the world overlay. Use a firewall / fail2ban for a public listener.

### Other behaviour changes

- RCON: the password is compared in constant time; failures are logged with
  the source IP (never the password), as is each executed command.
- Join/leave log lines carry the remote IP; refused connections are logged
  (at most one line per second).
- The tick thread runs under `catch_unwind`. A panicking tick logs
  `[FATAL] tick thread panicked: ...` and exits with status 70 so a supervisor
  restarts the server from the last autosave, instead of leaving the listener
  alive with a dead tick. The world is deliberately not saved on that path (it
  may be half-updated). A panic also poisons every `Mutex` it held, and the
  ~970 `lock().unwrap()` calls would panic on them in turn; that is
  intentionally left alone: ending the process is the policy.
- Broadcasts: a client whose socket write fails or times out is shut down,
  so it is logged out by its own thread and later broadcasts fail instantly
  instead of each waiting out the 2 s write timeout on the tick thread. There
  is still no per-client bounded queue; one stuck client can still cost the
  tick thread one write timeout before it is dropped.
- Skin lookups: cache capped at 1024 entries (1 h TTL for hits, 5 min for
  failures, oldest evicted), keyed by lower-cased name, at most 4 lookups /
  threads in flight; a login that finds the pool busy simply has no skin.
- World saves: failures are logged with the path; the tmp file is fsynced; the
  backups are `world_overlay.bin.bak` (previous save) plus `.bak.1` and
  `.bak.2`, which only advance when `.bak.1` is at least
  `MC_SPINE_BACKUP_INTERVAL_SECS` old.
- Configuration-phase read loops return on EOF/timeout instead of
  `continue`-spinning at 100 % CPU.
