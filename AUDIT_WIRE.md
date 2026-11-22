# AUDIT_WIRE — megu OOM signature hunt (lane megu-audit-2)

Scope: play-state dispatch (`handle`, L4872–6128), persistence (`save_world`/`load_world`
L1163–1303), spawn paths, `thread::spawn` sites, tick fns (L374–1032), command executor
(L4496–4860), chunk streaming (L4034–4165), packet I/O (L45–75). main.rs was READ-ONLY.

Target signature: **67 GB VM / 20 GB RSS in ~100 s, only while a client is connected
(idle flat — verified)**.

## Verdict — signature decomposition

The VM≫RSS shape (~3.3× virtual over resident) plus "~100s" is the textbook signature of
the **pre-cap `read_packet`** (kept as `evidence/foundation/audit-snapshots/main.pre-cap.rs`):

- `vec![0u8; len]` compiles to `alloc_zeroed` → for multi-GB sizes glibc serves it via
  `mmap` → **VmSize jumps instantly, VmRSS stays ~0** (zero pages, untouched).
- `read_exact(&mut body)` then fills only what the client actually transmits →
  **RSS grows in lockstep with wire bytes received**. ~200 MB/s ingest = 20 GB in ~100 s.
- VM 67 GB ≈ two in-flight ~34 GB declared frames (VarInt max ≈ 34 GiB) — one per
  connection, or sequential frames while the first still holds its mapping.
- Requires an actively-sending client → flat when idle. Matches every term of the
  signature exactly.

On the **current** binary this path is dead (`len > 16 MiB` → shutdown, L54–57) and —
grep-verified — **no wire-derived `vec![0;n]`/`Vec::with_capacity`/reserve remains**, so
post-cap nothing can allocate large VM without touching it. If the 67/20 measurement is
post-cap, the retained-growth candidates below are the residual explanations, ranked by
wire→retained amplification. P1 is the only path that can reach GB-scale in ~100 s.

## P1 — `scores()` wire-string retention (STRONGEST residual, ~1–2× wire bytes retained)

`0x04|0x05` chat_command / `0x06` chat_message (L5536–5553) pass the raw message
(≤16 MiB under cap) to `run_command` → `run_command_ctx` → scoreboard branch:

- L4834 `"set"`: `sc.entry(obj.to_string()).or_default().insert(name.clone(), v)`
- L4835 `"add"`: same two wire-controlled Strings stored verbatim.
- L4812 `"objectives add"`: `entry(obj.clone())` — same.

`args[2]` (score-holder name) and `args[3]`/`args[2]` (objective) are **unbounded
wire-controlled Strings** (vanilla caps both at 16 chars; here ≤16 MiB each) inserted
into the global `HashMap<String, HashMap<String, i64>>` — **no eviction, no dedup
needed beyond uniqueness**. Each packet retains ≈ name+obj bytes ≈ up to ~2×8 MiB.

- Retained ≈ wire bytes → 20 GB RSS needs ~20 GB ingest ≈ 200 MB/s × 100 s — fits the
  measured window and the client-connected requirement exactly.
- Reply path `format!("Set {} = {}", name, v)` → `system_chat` → `send_packet` echoes
  the giant name back (transient; note `nbt_str` L4111 writes `s.len() as u16` —
  truncates >64 KiB strings = malformed NBT, separate correctness bug).
- `scores()` is never persisted — it's pure leak (no legitimate consumer reads it back
  except `players get`, `objectives list` joins keys — could also send GB back).

## P2 — per-connection `sent` HashSet, unbounded chunk accumulation (~160× wire)

L4946 `sent: HashSet<(i32,i32)>` lives on `handle`'s stack for the connection lifetime.
L4154–4157 inserts up to 81 chunk coords per call whenever the pos packet crosses a
chunk boundary (L5097–5102). Center is wire-derived `(px/16.0).floor() as i32` — a
teleport bot jumping ≥144 blocks/step lands in fresh 9×9 territory every packet:
~48 B/entry × 81 = **~4 KB retained per ~25 B position packet**, no bound, no eviction,
freed only on disconnect. Rate-bound by pos-packet throughput (the fartp phase of
probe_client.py exercises exactly this). Secondary: each new chunk also pays
`map_chunk` noise-gen CPU (~256 `height_at` + up to 24 `section_encode`), an
amplification DoS even before memory matters.

## P3 — HashMap/Vec never shrink: capacity pinning

- `set_block` (L1155–1159): re-writing a block to `base_state` `c.remove()`s the
  overlay entry but the per-chunk `HashMap<u32,u32>` **keeps bucket capacity forever**
  (global, survives disconnect). Dig/place churn at unique coords pins peak capacity.
- Same for `world()`'s outer map, `drops()/mobs()/xps()/arrows()/primed()` Vecs
  (capacity stays at peak; despawn only shrinks len), `chests()/furnaces()/crops()/
  inv_store()/scores()` (entry-level, no remove-on-empty anywhere).

## P4 — login `name` ≤16 MiB (no length cap; vanilla = 16)

L4895–4896 `nlen` varint → `from_utf8_lossy` with only `min(lp.len())` clamp. The name
is retained in `me.name`, written to `inv_store` on leave (L6117), cloned into
`save_world`'s `all` map every ~30 s (L1194–1195, transient 2× per save), and broadcast
via `player_info_add`. Reconnect bot with unique 16 MiB names → permanent ~16–32 MB
per join (plus each join replays `load_config_packets` file parse L4019 — 105 KiB
transient + full 81-chunk stream per connect).

## P5 — per-packet transient amplification (VmPeak inflator)

Chat flood: `sl.to_vec()` (L62) + `m` String (L5539) + `format!("/{}", m)` (L5540) +
`chat_nbt` Vec + `send_packet` body+out (L64–69) ≈ **5–8× packet size live**;
`broadcast` `pl.clone()` per remote client multiplies further (L163). All transient —
raises VmSize during the flood but frees after; can explain a large VM/RSS gap while
active but cannot alone reach 20 GB RSS.

## P6 — small-per-packet retainers (rate-bound, cannot reach GB in 100 s)

`window_click -999` → Drop ≈ ~90 B / ~2 pkts (cursor must be non-empty); `world()`
overlay ≈ ~50 B / dig-or-place pkt at unique coords; `chests()/furnaces()` entry per
click on a real placed block; `/summon` ≈ ~300 B/mob per chat pkt (`name_to_etype` is
static-table so etype itself is free); bobbers ≤1 live per owner (reel/disconnect
removes); pearls ttl 50, arrows ttl 60, primed 4 s fuse, drops/xp despawned at 5 min
(L790–798, L526–534 — despawn fixes verified present).

## Verified negatives (already fixed / not OOM)

- 64-handler accept cap L6171; 2 s read/write timeouts L5043–5044 (cloned sock shares
  SO_SNDTIMEO → broadcast writes also bounded ~2 s).
- `broadcast` snapshots `clients()` (L160) — no lock-order cycle; residual is stall
  only (≤2 s per client), not memory.
- `load_world` (L1232–1299): unchecked `b[p..p+4]` indexing → panic on truncated/corrupt
  `world_overlay.bin` (DoS via crash loop, not OOM — still unguarded).
- `run_function_depth` caps depth≤16 (L3994); fan-out unbounded but content is static
  datapack files, not wire.
- Datapacks on disk are benign (4 small functions, no recursion).
- `explode_at` crater is fixed ±2 cube regardless of `power` (L627–632).
- `mob_tick` early-returns with zero clients (L805) — consistent with idle-flat.

## Suggested confirmation probe (owner's call)

Drive `chat_command` frames `/scoreboard players set <uniq~1MiB> <uniq~1MiB> 1` at speed
under `systemd-run --user --scope -p MemoryMax=4G`: if P1 is live, RSS ≈ ingest bytes
and the scope OOMs quickly. Independent control: far-teleport spam (P2) should show
slower steady `sent` growth; idle stays flat.

## Bottom line

- **Best fit for 67 GB/20 GB/~100 s: the pre-cap `read_packet` alloc_zeroed+read_exact
  path.** If the measurement predates the 16 MiB cap (or ran on a stale binary — the
  notes warn old processes persist until restart), it is fully explained and fixed.
- **If measured post-cap: P1 `scores()` is the only ≥MiB-per-packet retained path and
  fits the timescale.** Cheapest fix: clamp scoreboard keys to vanilla lengths
  (objective ≤16, holder ≤40) before `.to_string()`, and/or cap `scores()` total
  entries/bytes. P2 wants a `sent` budget (e.g. drop conn or stop tracking past N
  chunks) and a per-connection chunk-stream rate limit.

---

# Audit-3 addendum (2026-09-26, lane megu-audit-3)

## The geometric single-mapping signature: identified

The 2026-09-25 captures (`night-2026-09-25/oom-captures`, pid 3329328 @17:04, pid 2035206 @16:29) show ONE anonymous rw mapping mremap-doubling 4→8→16→32 GiB in ~24 s while RSS plateaus ~6 GB (zram + swappiness=150 reclaims cold head pages as fast as the tail is written; push rate ≈1 GB/s ⇒ VM ≈67 GB at ~60 s). That is amortized `push` growth inside a single `Vec<u8>` — not a packet-length alloc, not a HashMap/HashSet (those distribute across many arenas), not count-keyed loops (every loop bound in the current source is now literal- or container-size-bounded — see below).

### Root cause: pre-fix `write_varint` infinite loop on negative i64

Incident-era builds (the 15:17 pre-cap source and the 16:41 installed-patch copy, both kept in
`evidence/foundation/audit-snapshots/`, and both predating the fix) carried:

```rust
fn write_varint(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let mut b = (v & 0x7f) as u8; v >>= 7;   // arithmetic shift on negative i64
        if v != 0 { b |= 0x80 } out.push(b);
        if v == 0 { break }                    // never reached: -1>>7 == -1
    }
}
```

Any negative `v` pushes `0xFF` forever into a fresh `Vec<u8>` → capacity doubles 1→2→…→32 GiB on one mapping; `send_packet` is never reached (the Vec is still being built); the handler thread spins at 100% CPU while other connections stay live — matching the ESTAB sockets and flat-idle behavior in the captures.

Current source is FIXED (L23–32): `v<0 && v>=i32::MIN → v as u32 as u64` (correct 5-byte VarInt for negatives), wider negatives → `v as u64` (10-byte VarLong). The comment documents this exact failure. Verify `target/release/megu` (and any running pids / `megu-inst` binaries — inst pid 2471559 still carries the bug) postdate the fix; a stale binary still OOMs.

### Negative-value sinks (all client-connected-only ⇒ idle/probes flat)

| Site | Value | Wire/state source |
|---|---|---|
| `stream_chunks` L4461 | `ccx`,`ccz` | Player position; NEGATIVE when player walks/tp's to x<0 or z<0 — fires on NORMAL play, no malformed packet needed. Highest-frequency sink (every chunk-crossing pos pkt). Explains "some runs": probes all used x≥0. |
| `ack(seq)` L5192 ← 0x24 dig L5469, 0x38 place L5555, 0x39 use_item L5812 + ~15 inner sites | `seq` | Raw `read_varint` i64 echoed verbatim — client sends seq<0 (fuzzer/desync/malformed) → instant runaway. |
| `set_creative_slot` 0x32 L6163–6164 | `cnt`,`item` | Raw wire i64s echoed unconditionally; storing item<0 also poisons `me.inv` → every later 0x15 slot echo (L991, L5354) re-loops forever. |
| sheep-graze L1142 | packed pos `bx<<38|…` | bx<0 ⇒ sign bit set ⇒ negative. Ambient mob_tick path — gated by `cls.is_empty()` early-return, hence client-connected-only. |

## Remaining audit items (current source, lower severity)

- `run_execute` has NO recursion-depth cap (`run_function_depth≤16` covers datapacks only). `/execute run execute run …` recurses building ~100 MiB live `args` Vecs per level → abort. Different signature (many mappings, SIGABRT) but a real crash path — add a depth guard.
- Rate-bound retainers still unbounded in principle: `world()` overlay, `mobs()`, `drops()`, `crops/furnaces/chests/inv_store` — per-packet inserts, no global cap; days-long accretion risk, not the incident signature.
- `RxBuf` is fully bounded (≤16 MiB+5 B per connection); `sent` cleared at 4096; scoreboard keys/entry-count capped; `window_click` mode≠0 ignored. No remaining wire-count→container loop found in current source.
- `load_world` panic-on-truncated-file (crash, not OOM) still unguarded; `xp_level` overflow spin latent but unreachable at realistic xp.

## Reproducer sketch (do NOT run unconstrained — OOMs the host)

```python
# seq=-1 on use_item (0x39): payload = varint(hand=0) + varint(seq=-1)
# -1 as 5-byte VarInt LEB128 is FFFFFFFF0F (parser accepts i>5, up to 10 bytes)
b = b'\x00' + b'\xff\xff\xff\xff\x0f'
# frame: varint(len(b)+1) + \x39 + b  → buggy build: handler pins a core,
# Vec doubles ~1GB/s → guard must kill <8GiB RSS. On fixed build: 5-byte echo, flat.
```
Run only under `systemd-run --user --scope -p MemoryMax=4G` against a scratch port. Equivalently tp a real client to `~-100 ~ ~` on a buggy build and walk west — `stream_chunks` triggers it with vanilla traffic.

---

# Audit-3 verification pass (2026-09-25, second sweep — src/main.rs @18:09, 6647 lines)

Systematic re-check of the full file for "wire/state-derived count feeds a growing
Vec/HashMap/String", since the file was live-edited after the first addendum.
Method: enumerated every `read_varint` consumer, every `for`/`while`/`loop` bound,
every alloc primitive, every container iteration. Snapshot kept at
`evidence/foundation/audit-snapshots/main.2026-09-25T1809.rs` (kept in the repository; the hashes and what these files are not are in that directory's README).

## Verdict: no remaining wire/state-count → container-growth path exists in current source

- `write_varint` fix verified present and correct (L24–32): i32-range negatives →
  `v as u32 as u64` (≤5 iters); wider negatives → `v as u64` (≤10 iters);
  non-negative → ≤10 iters. Always terminates — closes the whole negative-sink
  class (ccx/ccz, seq echo, creative-slot item, packed pos) regardless of caller.
- Alloc primitives: **zero** occurrences of `with_capacity`/`reserve`/`resize`/
  `set_len`/`repeat` in the file. No single-call path can size an allocation off
  a parsed count.
- Every `for _ in 0..X` bound is a literal or a `load_world` file count
  (n/m/mn/pn/en/cn, L1512–1576). The file counts look unbounded but each
  iteration consumes bytes via unchecked `b[p..p+N]`/`b[*p]` indexing — a
  corrupt count panics on OOB long before memory grows (crash, not OOM;
  startup-only anyway).
- Wool-drop `n` (L5963) = `1 + hash%3` ∈ 1..=3. `trades()` is a static table.
  `resolve_selector`/snapshot idiom used everywhere — no iterate-while-insert
  on the same container. `while i < pl.len()`/`args.len()` are remove/iterate.
- Confirmed caps now live in source: scoreboard keys `.chars().take(64)` +
  ≤4096 entries/objective + ≤1024 objectives (L5135–5167); login name
  `.take(64)` (L5228) — so the `@s`/`@p` scoreboard-key clamp-bypass noted in
  the earlier draft is CLOSED (name is clamped at login); `sent` clears >4096
  (L4467); RxBuf ≤16 MiB+5 B; `read_packet` ≤16 MiB transient; hostile spawn
  cap `mcount < 12` (L537); drops/xp 300 s despawn + arrow/pearl/bobber/ttl
  retains all still present.
- `nlen`/`alen`/`mlen` all `.min(slice.len())`-clamped; no `as usize` reaches an
  allocation or loop bound unclamped.

## New surface since the 17:41 snapshot (owner edit @18:09) — reviewed

- `hoppers()`: new global `HashSet<(i32,i32,i32)>`; `set_block` inserts on
  hopper-state placements (L1434–1436), `hopper_tick` (L878–923) removes only
  when the block no longer exists. ~24 B per unique hopper coord, **unbounded
  in principle** — same rate-bound accretion class as the world overlay, not
  the single-mapping signature. A hopper-spam bot (creative place at unique
  coords) grows it ~24 B/pkt.
- Ender chests (L5572–5577): `chests()` gains a synthetic `ender_key` entry per
  distinct player UUID — bounded by joiner count × ≤27 slots, but UUID-keyed
  entries are never evicted → accretion-class.
- Flint&steel on TNT → `primed().push` (L5580–5589) — bounded by 4 s fuse.
- `tp` by-name lookup (L4990–4993) — read-only.
- New `eprintln!` per non-movement packet (L5394) and per chat packet (L5931) —
  stderr/journald volume under flood, not heap.

## If the signature still reproduces on a current-source build

It cannot be a wire/state count in this source. The geometric 2×-capacity
single-mapping + ~1 GB/s tail-write rate in the captures points uniquely at
`Vec<u8>` push-doubling, and the only such unbounded loop ever present was
`write_varint` (fixed). Check in order: (a) stale binary/process — any pid
predating the fix (or `megu-inst` copies) still OOMs; (b) reproduce under
`systemd-run --user --scope -p MemoryMax=4G` and capture `/proc/<pid>/smaps` +
`gdb -p <pid> -batch -ex 'thread apply all bt'` mid-growth — the writing
thread's frame names the Vec owner directly; (c) `x<0` vanilla play is the
cleanest reproducer class (`stream_chunks` ccx/ccz) — probe scripts used x≥0
only, which explains "some runs".

## Standing residuals (unchanged, non-signature)

- `run_execute` has no recursion-depth cap (`/execute run execute …`) → deep
  recursion builds per-level arg Vecs → SIGABRT-class crash, not this signature.
- `load_world` unchecked `b[p..p+N]` → panic on truncated/corrupt
  `world_overlay.bin` (crash loop).
- `xp_level` `while total >= acc + next` (L305): iteration count grows
  ~√total — CPU spin only, no allocation; unreachable at realistic xp.
- `nbt_str` writes `s.len() as u16` — strings >64 KiB produce malformed NBT
  (client-side parse failure; correctness bug, not memory).
