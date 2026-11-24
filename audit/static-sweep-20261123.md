# Static Bug-Signature Sweep — 2026-11-23

Repo: `<repo>` @ `8b00b7d2` (megu-mc, Minecraft protocol 767 / 1.21.1).
Method: `rg` + targeted reads only. No builds, no tests. Test-only code excluded.
Reachability was verified by tracing callers, not by matching symbol names.

**Headline:** the wire-facing decode paths are unusually well hardened (frame caps,
NBT budgets, component limits, bounds-checked Anvil/save readers). No confirmed
Critical/High remote crash or unbounded wire allocation was found. The confirmed
findings are three conditional panic paths, two truncation-cast sites, and a set
of finished-but-unwired production features.

---

## Ranked findings

### Medium

**M1 — PANIC-ON-WIRE: `.expect()` on End-dimension portal target**
[src/play_protocol.rs:6655](../src/play_protocol.rs#L6655)

```rust
.expect("only the End has no Nether portal")
```

`enter_portal` is reached from `tick_portal` (called every tick from the play loop,
[src/play_protocol.rs:10209](../src/play_protocol.rs#L10209)) when a
player stands inside a `nether_portal` block state. The `else` branch (non-End,
non-Nether dims) calls `nether_portal_target(DimId::End).expect(...)`. If a
`nether_portal` block exists in the End dimension — placeable by an op via
`/setblock`/`/fill`, allowed inside structure templates
(`template_structures.rs` whitelists `minecraft:nether_portal`), or present in a
crafted save — the player touching it panics. The panic unwinds to the
per-connection `catch_unwind` at [src/main.rs:20222](../src/main.rs#L20222)
→ `disconnect_cleanup`; no mutex is held at the panic site (`dim`/`in_portal`
guards are temporaries, `drop(at)` precedes), so impact is a targeted kick of
that connection, not server-wide corruption. Requires unusual world state, hence
Medium rather than High.

**M2 — PANIC-ON-WIRE (untrusted file): unclamped `DIFFICULTY` byte → OOB array index**
[src/save_v2.rs:4767](../src/save_v2.rs#L4767) stores the raw save
byte:

```rust
DIFFICULTY.store(b[p], Ordering::Relaxed);
```

Every other writer clamps or validates (`set_difficulty` rejects `>3`,
[src/commands_core.rs:5848](../src/commands_core.rs#L5848);
anvil load clamps `0..=3`, [src/anvil_io.rs:1179](../src/anvil_io.rs#L1179);
play packet rejects `b[0] > 3`). The save_v2 path does not. A crafted/edited save
with difficulty byte `>3` then makes the `/difficulty` query panic at
[src/commands_core.rs:8139](../src/commands_core.rs#L8139):

```rust
["peaceful","easy","normal","hard"][DIFFICULTY.load(Ordering::Relaxed) as usize]
```

All other `DIFFICULTY.load` consumers use `match` with `_ =>` fallthrough — this
index is the only panic site. Also note [src/anvil_io.rs:1120](../src/anvil_io.rs#L1120)
`DIFFICULTY.load(...) as i8` wraps values `128..=255` on the next save, silently
corrupting the byte further. Severity Medium (requires loading a hostile save;
panic is per-connection via catch_unwind but the corrupted byte persists).

### Low

**L1 — PANIC-ON-WIRE: TOCTOU `unwrap()` on actor entity lookup**
[src/commands_core.rs:8749](../src/commands_core.rs#L8749)

```rust
a.get(args[2]).unwrap()
```

`contains_key` was checked under one lock guard which is then dropped; `get` runs
under a fresh guard. If the actor entity is despawned/removed between the two
locks (entity tick, `/kill`, chunk unload), `get` returns `None` → panic → the
issuing connection is disconnected by the outer `catch_unwind`. Op-gated command
path; narrow race window. Low.

**L2 — TRUNCATION CAST: NBT string length `as u16` (latent)**
[proto/src/item_components.rs:991](../proto/src/item_components.rs#L991)

```rust
w.write_u16(value.len() as u16);
```

`put_nbt_string` silently wraps string lengths ≥ 65536. Today every input reaches
it through `sanitize_text_component`, which bounds payloads at
`MAX_COMPONENT_BYTES = 4096` ([proto/src/item_components.rs:15](../proto/src/item_components.rs#L15)),
so it cannot trigger — but it is the same bug class as the known
`nbt_str`/`s.len() as u16` example and will silently corrupt component bytes if a
future caller bypasses the 4 KiB cap. Sibling at line 1001,
`w.write_i32(items.len() as i32)`, is bounded by `MAX_LIST_ENTRIES = 64` — safe.
Low / latent.

**L3 — TRUNCATION CASTS: `as u16`/`as u32` on wire counts in live-mod bridge**
[src/live_mod_bridge.rs](../src/live_mod_bridge.rs) — ~10 sites
in the 2990–3340 range (e.g. length/count fields serialized as `as u16`/`as u32`).
Counts above 65535/2^32 wrap and corrupt the bridge frame. Reachability requires
the `mod-suite`/`create-offload` feature and a mod adapter emitting oversized
counts; the data is adapter-produced rather than directly remote-wire, so
severity is Low and feature-gated.

**L4 — DEAD/UNWIRED: finished anti-xray masking path never called**
[src/antixray.rs:48](../src/antixray.rs#L48) `mask_chunk` and
[src/antixray.rs:104](../src/antixray.rs#L104) `obfuscate_chunk`
(~110 lines incl. decoy/filler logic) are annotated
`#[cfg_attr(not(test), allow(dead_code))]`; their only callers are the module's
own tests. Production anti-xray is implemented inline via `is_hidden` /
`hidden_filler` / `block_exposed` instead, so this is a parallel finished
implementation left unwired — either wire it or delete it before it rots.

**L5 — DEAD/UNWIRED: enchantment/melee combat cluster (test-only callers)**
[src/combat_and_health.rs](../src/combat_and_health.rs):
`enchantment_targets_aquatic` (:621), `typed_damage_bonus` (:646),
`bane_slowness_iv` (:663), `entity_is_living` (:691), `melee_hit_damage` (:741),
`ignite_mob_fire_aspect` (:773), `mob_fire_ticks_of` (:780), `tick_mob_fire`
(:796), `sweeping_attack_gate` (:823), `sweeping_movement_ok` (:829). All carry
`#[cfg_attr(not(test), allow(dead_code))]`; every call site is inside `#[cfg(test)]`
modules (lines ≥4937). This is a complete vanilla-faithful damage/enchantment
model that production combat never calls — finished feature, unwired.

**L6 — DEAD/UNWIRED: save-format dead getters**
[src/anvil_io.rs:846–886](../src/anvil_io.rs#L846) — `data_version`,
`version_id`, `level_name`, `difficulty_locked`, `game_type` accessors on the
level record; no production callers.

**L7 — Systemic note (amplifier, not a bug): mutex poison cascades**
The codebase mixes `.lock().unwrap()` with `.lock().unwrap_or_else(|e|
e.into_inner())`. Plain `.unwrap()` sites are widespread; any panic that escapes
while holding a `Mutex` poisons it, and every subsequent plain-`unwrap` lock on
that mutex panics too — turning one conditional panic (M1/L1) into cascading
failures on shared state (`world()`, `tags()`, `mobs()`). The outer
`catch_unwind` contains the blast radius per connection, but shared-mutex poison
can outlive it. Worth a sweep toward `into_inner` recovery or `parking_lot`.

---

## Per-class counts

| Class | Confirmed findings | Notes |
|---|---|---|
| PANIC-ON-WIRE | 3 (M1, M2, L1) | All conditional; all contained by per-connection `catch_unwind` |
| WIRE-DERIVED ALLOCATION | 0 | Every `with_capacity`/`vec![0;n]`/`repeat` site checked is capped or bounded by input length (see below) |
| TRUNCATION CASTS | 2 (L2 latent, L3 feature-gated) | `nbt.rs` length casts verified bounded |
| CHUNK-BOUNDARY ARITHMETIC | 0 | Coordinate decomposition verified consistent, incl. negative coords |
| DEAD/UNWIRED CODE | 3 clusters (L4–L6) + systemic | ~2,381 `allow(dead_code)` sites total; top production clusters listed |

## Verified-safe highlights (checked and cleared)

- **Frame ingress**: `RxBuf::try_frame` rejects `len == 0 || len > MAX_PLAY_FRAME`
  before allocating the body; login frames capped at `MAX_LOGIN_FRAME = 64 KiB`;
  play length prefix limited to 3 VarInt bytes.
- **Compressed frames**: `connection_limits::decode_compressed_frame` takes an
  explicit max decompressed size.
- **NBT**: `src/nbt.rs` checks every collection/string length against remaining
  input, enforces depth and tag-count budgets; all `read_gzip` callers cap at
  16 MiB; `number()`'s `try_into().unwrap()` is structurally safe after exact
  slicing.
- **Item components**: `proto/src/item_components.rs` bounds nested stacks,
  component lists (`MAX_LIST_ENTRIES=64`), books, charged projectiles, container
  depth, and total component bytes (`MAX_COMPONENT_BYTES=4096`,
  `MAX_STACK_COMPONENTS_BYTES=8192`); creative-slot path adds
  `SLOT_COMPONENT_WIRE_LIMIT=8192` plus sanitization/clamping.
- **`unreachable!` on `Nbt::Ignored`** (item_components.rs:986,1014): verified
  unreachable — `sanitize_nbt`'s catch-all converts `Ignored`/`Byte` to an empty
  text compound before serialization.
- **Paletted `unreachable!`s**: guarded by the internal bit-width invariant
  (4–8/15) established at decode/construct time; index math masked to 4096.
- **Chunk-boundary arithmetic**: `x >> 4` / `x & 15` / `div_euclid(16)` used
  consistently in `lighting.rs` neighbor propagation, `main.rs` cell packing,
  and `worldgen.rs`; `local_idx` ([src/main.rs:9491](../src/main.rs#L9491))
  uses `checked_sub(MIN_Y)` and returns `u32::MAX` for out-of-range y instead of
  wrapping; portal `floor() as i32` scaling is float-safe for encodable coords.
- **`brigadier.rs` "Can't find node before cursor"**: child suggestion contexts
  always start ≤ parent end < cursor, so the `unreachable!` holds.
- **Dimension JSON** (`worldgen.rs:833–839`): `canonical_key` always returns
  `ns:path` form, so `split_once(':').unwrap()` cannot fail on validated input.
- **Anvil/save readers**: fixed section sizes, `bits` validation, and length
  checks throughout; ZIP/datapack limits (`ZIP_MAX_*`, `DPC1_MAX_*`) enforced.
- **`play_protocol.rs:883` `read_pos` `try_into().unwrap()`** and
  `ingress_guard.rs:50–62` fixed-offset conversions: all callers check
  `b.len() >= 24` (movement) before dispatch; safe.
- **`main.rs:14239` `vec![0u32; rows << 8]`**: `rows` derives from a bounded
  dimension band range, not wire input.
- **`play_protocol.rs:933` body allocation**: length already capped by
  `MAX_LOGIN_FRAME`/`MAX_PLAY_FRAME` before `vec![0u8; len]`.

## Method notes / limitations

- `tools/rust_module_graph.py` was not re-run this pass; dead-code findings were
  established by direct call-site search (single-file references confined to
  `#[cfg(test)]` modules), which is stronger evidence than module-graph edges.
- ~214 `vec![0; ...]` sites in `src/` were sampled by risk file
  (`play_protocol.rs`, `main.rs`, `nbt.rs`, `paletted.rs`, `anvil_io.rs`,
  `proto/src/*`) rather than exhaustively; all sampled sites were bounded.
- Findings assume the documented `catch_unwind` containment at
  [src/main.rs:20222](../src/main.rs#L20222); if per-connection
  panic containment is ever removed, M1/L1/M2 become full server crashes.
