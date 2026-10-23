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
- No authentication/rate-limit on the listener — unchanged.

## Remaining findings (earlier pass)

- `world` overlay grows ~16 B per edited block, unbounded by design
  (it IS the world save). A bot could spam dig/place packets to grow
  it — the cheapest structural mitigation is a per-chunk diff cap or
  accepting that persistence is intended growth.
- One thread per client — now capped at 64 live handlers; excess
  connections are dropped at accept time.
- No authentication/rate-limit on :25599 — any LAN host can drive the
  above paths. Recommend binding behind firewall or adding a cheap
  connection throttle.
- String reads are length-clamped by `.min(sl.len())` — safe pattern,
  keep it if touched.
