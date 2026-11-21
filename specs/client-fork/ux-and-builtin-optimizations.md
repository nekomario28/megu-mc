# Lane U3 — Built-in optimization superseding + low-cognitive-load client settings

This document fixes two UX contracts:

1. **Operators**: a real CurseForge/Modrinth modpack folder must not force
   Tier 3 NeoForge fallback just because it ships redundant performance mods.
2. **Players**: the settings screen only shows choices that change what the
   player sees or that trade something off. Every zero-downside engine
   optimization is always on and has no toggle at all.

Implementation: [mod-adapter-core/src/fallback.rs](../../mod-adapter-core/src/fallback.rs),
[tools/mod_launcher.py](../../tools/mod_launcher.py),
[tests/mod_fallback_test.py](../../tests/mod_fallback_test.py),
[client-app/src/settings.rs](../../client-app/src/settings.rs).

## 1. Built-in optimization superseding

`mc-spine` natively implements the whole optimization stack that popular
performance mods provide. When `mods/` contains them, loading their JARs
buys nothing and their heavy Mixins previously forced Tier 3
`DelegateToNeoForge` — dragging *every* mod in the pack onto the JVM.

The fix is an id catalog, `BUILTIN_SUPERSEDED_PERF_MODS`, checked **before**
flagship matching, ShimCheck, and all tier logic. A mod whose declared mod
ids (Fabric or NeoForge/Forge, case-insensitive) intersect the catalog is
classified `ExecutionTier::BuiltInSuperseded`:

- never ShimCheck-scanned (its Mixins are never even read),
- never counted toward Tier 3, so it can never trigger
  `DelegateToNeoForge` or a `Strict` abort,
- never copied into quarantine staging,
- reported once to the operator:

  ```
  Built-in optimizations replace 3 mod(s) (skipped automatically): lithium, modernfix, sodium
  ```

  (Rust side: `StartupPlan::superseded_notice()` prints
  `Skipped 3 redundant performance mods (already built into mc-spine): lithium, modernfix, sodium`.)

Real content mods in the same pack (`create`, `aether`, `ae2`, …) are
unaffected and keep 100% pure native Rust execution.

### Coverage matrix

| Superseded mod ids | What they did | mc-spine built-in |
| --- | --- | --- |
| `sodium`, `embeddium`, `rubidium`, `xenon`, `magnesium` | Chunk mesh pipeline, compact vertex formats, graph culling | `client-port` greedy mesher + S1/S2 `GraphCuller` + packed 16-byte `PackedVertex` |
| `immediatelyfast` | HUD/text batching | Batched HUD submission (`BuiltInOptimizations::fast_hud_batching`) |
| `entityculling`, `moreculling` | Entity/block-entity and extra face culling | `entity_culling` + S3 direction-bucket backface culling |
| `distanthorizons`, `voxy`, `bobby`, `farsight` | Distant LOD terrain / chunk retention | `client-app/src/lod.rs` `LodManager` (fine/coarse `LodCell` summaries, byte-budgeted eviction) + `DistantHorizon` setting |
| `indium`, `reeses-sodium-options`, `sodium-extra`, `embeddium_extra` | FRAPI bridge / Sodium option UIs / extras | Not needed: the native pipeline and `settings.rs` cover their function |
| `lithium`, `radium`, `canary` | Tick/physics/data-structure optimizations | Native Rust tick scheduler in `mod_offload::RegionScheduler` |
| `starlight`, `phosphor`, `scalpel` | Light engine rewrite | `client-kernel` light propagation + sparse `cull_chunk_light` masks |
| `ferritecore`, `saturn` | Blockstate/model memory dedup, compact palettes | `PalettedSection` compact storage + `deduplicated_resources` |
| `modernfix`, `smoothboot`, `fastload`, `dashloader` | Startup/memory/threading fixes | Native startup path has no JVM warmup to fix |
| `noisium`, `c2me` | Worldgen noise/threading | Native worldgen pipeline + parallel chunk workers |
| `krypton` | Network stack | Native protocol-767 writer + `ChunkPacketCache` frame sharing |
| `lazydfu`, `memoryleakfix`, `servercore` | DFU warm-up, leak fixes, server trims | No DFU/JVM; Rust allocators + bounded caches (`MAX_*` caps) |
| `alternate-current`, `clumps` | Redstone power order, XP-orb merging | Native redstone engine; entity-merge rules in the tick loop |

Adding a new built-in optimization means extending this catalog — never
maintaining compatibility shims for mods we already outperform.

## 2. Low-cognitive-load player settings

`client-app/src/settings.rs` exposes exactly ten concepts:

| Setting | Type / range | Default |
| --- | --- | --- |
| `preset` | `fast` / `balanced` / `high` / `custom` | `balanced` |
| `view_distance` | 6–32 chunks | 12 |
| `distant_horizon` | `off` / `medium` (64) / `far` (128) | `off` |
| `clouds` | `off` / `fast` / `fancy` | `fancy` |
| `particles` | `minimal` / `decreased` / `all` | `all` |
| `smooth_lighting` | bool | true |
| `entity_shadows` | bool | true |
| `fov` | 30–110 | 70 |
| `vsync` | bool | true |
| `max_fps` | `unlimited` or 30–260 | `unlimited` |

Rules:

- **No developer toggles.** S1/S2 graph occlusion culling, S3 backface
  bucket culling, S4 multidraw region batching, S5 frustum-prioritized
  rebuilds, compact palettes, deduplicated resources, fast HUD batching,
  and entity culling are always on via
  `ClientSettings::engine_optimizations() -> BuiltInOptimizations`, which
  is intentionally not populated from the user config.
- `to_lod_config()` maps `view_distance` + `distant_horizon` directly onto
  `LodConfig` (`full_detail_radius = view_distance`, coarse radius = 64 or
  128, fine at the midpoint — the `full <= fine <= coarse` invariant holds
  by construction).
- TOML round-trips losslessly (`to_toml`/`from_toml`, pure std). Unknown
  keys and out-of-range values degrade to warnings + clamping, never to a
  crash.
- `import_vanilla_options_txt` migrates an official-launcher profile in one
  call: `renderDistance`, `fov` (0–1 slider or degrees), `particles`,
  `renderClouds`, `enableVsync`, `maxFps` (260 = unlimited), `ao`,
  `entityShadows`, `graphicsMode`.

## 3. Clean-room & licensing policy

1. **All built-in optimizations in `mc-spine`, `client-port`, `client-kernel`,
   and `client-app` are 100% clean-room original Rust implementations.**
   Zero code, shaders, assets, or logic is ported, translated, or copied
   from any third-party optimization mod.
2. **`BUILTIN_SUPERSEDED_PERF_MODS` is a skip list, not a dependency.**
   Matching a mod id string against it solely *skips loading* a
   user-supplied JAR. It does not copy, link, bundle, load, or derive from
   those mods in any way — the JAR is never opened beyond reading its own
   declared mod ids, and no entry of it is ever executed or staged.
3. **Contributors must never read/port/copy code from source-available or
   all-rights-reserved mods.** In particular **Sodium 0.6+** is licensed
   **PolyForm Shield 1.0.0** (source-available, non-OSI, incompatible with
   GPL/LGPL family copyleft), and **OptiFine** is all rights reserved.
   Studying their implementations to replicate behavior is prohibited;
   behavior parity is established from observable semantics and Vanilla
   contracts only.
4. Because everything superseded is clean-room, `mc-spine`'s own
   `GPL-3.0-or-later` / `LGPL-3.0-or-later` / `AGPL-3.0-or-later` crate
   licenses remain 100% unencumbered regardless of how the skipped mods
   are licensed.

### Third-party license awareness table

Licensing of the superseded mods, for contributor awareness only (nothing
here is consumed):

| Mod(s) | License posture | Note |
| --- | --- | --- |
| Sodium 0.6+ | **PolyForm Shield 1.0.0** (source-available, non-OSI, GPL-incompatible) | Never read/port/copy; superseded by clean-room S1–S5 pipeline |
| Embeddium (Sodium fork, 0.3+ base varies) | LGPL/PolyForm-lineage | Same rule: never consult sources |
| OptiFine (not in catalog; closed-source) | All rights reserved | Never consult; clean-room only |
| Lithium | LGPL-3.0 | Clean-room regardless |
| Starlight (Phosphor lineage) | LGPL-3.0 | Clean-room regardless |
| ModernFix | MIT-or-LGPL multi | Clean-room regardless |
| FerriteCore | MIT | Clean-room regardless |
| ImmediatelyFast | LGPL-3.0 | Clean-room regardless |
| EntityCulling | LGPL-3.0 | Clean-room regardless |
| Distant Horizons | LGPL-3.0 | Clean-room regardless |
| C2ME | MIT | Clean-room regardless |
| Krypton, LazyDFU, MemoryLeakFix, Alternate Current, Clumps, MoreCulling, Noisium, Voxy, Bobby, Farsight, Saturn, SmoothBoot, Fastload, DashLoader, ServerCore | MIT/LGPL/ARR-mixed per upstream | All treated identically: id-match → skip; never read, copy, or port |

Rule of thumb for reviews: a diff that adds a mod id to
`BUILTIN_SUPERSEDED_PERF_MODS` must come with evidence that `mc-spine`
already implements the function natively (code + tests), not code taken
from that mod.
