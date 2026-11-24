<!-- SPDX-License-Identifier: LGPL-3.0-or-later -->

# Universal Dimension-Adding Mod Support

Status: **`mod-adapter-core/src/dimension_mod.rs` + launcher awareness implemented.** Covers The Aether, Twilight Forest, Ad Astra, Deeper & Darker, The Undergarden, Blue Skies, Bumblezone and structurally identical dimension mods. Reference: [`dimension_mod.rs`](../../mod-adapter-core/src/dimension_mod.rs), [`tools/mod_launcher.py`](../../tools/mod_launcher.py), [`tests/mod_fallback_test.py`](../../tests/mod_fallback_test.py). Parent contract: [universal-mod-adapter.md](universal-mod-adapter.md).

## 1. Position on Track 2 dynamic dimensions

megu already owns the four pieces a custom dimension needs at runtime:

- `DimId(u32)` + append-only `DimensionRegistry` (`src/worldgen.rs`): custom `dimension_type` JSON registers a slot ≥ 3 with `min_y`, `height`, `coordinate_scale`, `has_skylight`, `has_ceiling`, `ultrawarm`, `bed_works`, `respawn_anchor_works`, `effects`.
- `0x07` Registry Data + custom `dimension_type` NBT in the Configuration phase so unmodified clients learn the dimension.
- `DXT1` Save v2 footer carrying custom-dimension chunks keyed by canonical `"namespace:path"`.
- `/execute in` and portal hops applying each dimension's `coordinate_scale`.

Dimension-adding mods are datapack-shaped content plus (sometimes) Java `ChunkGenerator` code. `mod-adapter-core` therefore treats them with the same zero-JVM pipeline as recipes and tags: read the JAR's `data/` entries, classify, and run what is provably data-driven natively.

## 2. Data discovery: `discover_dimension_mod_entries`

`mod-adapter-core/src/dimension_mod.rs` scans `(path, bytes)` entries from a mod JAR or datapack directory (pure `std`, reusing `data_loader::parse_json` — no crates-io deps):

| Entry glob | Extracted datum |
| --- | --- |
| `data/<ns>/dimension_type/*.json` | `DimTypeDescriptor` (`min_y`, `height`, `coordinate_scale`, `has_skylight`, `has_ceiling`, `ultrawarm`, `natural`, `bed_works`, `respawn_anchor_works`, `ambient_light`, `effects`) |
| `data/<ns>/dimension/*.json` | `DimEntryDescriptor` (`id`, `type_id`, `generator_kind`: `Noise` / `Flat` / `Void` / `CustomChunkGenerator(type)`) |
| `data/<ns>/worldgen/{noise_settings,biome,configured_feature,placed_feature,structure}/*.json` | `WorldgenDataSummary` counts and canonical ids |
| `data/<ns>/tags/{block,blocks}/ores*.json`, `.../chests*.json` | Concrete ore/chest block ids — forwarded verbatim into the anti-xray hidden set (same contract as `sync_antixray_tags`, which already honours `#c:ores`/`#c:chests` roots). Nested `#tag` members are left for the tag table. |

Classification (`DimensionModData::classification`):

- **Pure data-driven** — every dimension uses `minecraft:noise`, `minecraft:flat`, or `minecraft:debug`: the mod is equivalent to a datapack and runs on the native Track 2 path. **Tier 1 `UniversalNative`, zero JVM.**
- **`CustomChunkGenerator`** — at least one `dimension/*.json` names a modded generator type: the native generator cannot reproduce it and the bounded shim exposes no `ChunkGenerator` surface. The mod reports `CUSTOM_CHUNK_GENERATOR` and falls to Tier 3 (`delegate`/`quarantine`/`strict` policy applies as usual).

Malformed `dimension*`/`dimension_type*` entries are recorded in `skipped_malformed`; they never abort the whole scan.

## 3. Declarative mod portals: `PortalRule` / `PortalRegistry`

Vanilla encodes exactly one portal shape. Dimension mods use four patterns; all are declared, not hard-coded:

```text
PortalTrigger::VerticalFrame      glowstone+water (Aether), reinforced_deepslate
                                  (Deeper & Darker), stone_bricks+catalyst
                                  (Undergarden), obsidian+flint (vanilla)
PortalTrigger::HorizontalPool     2x2 water + flowers + dropped diamond
                                  (Twilight Forest)
PortalTrigger::AltitudeThreshold  rocket ascent y >= 600 (Ad Astra,
                                  Create: Liftoff)
PortalTrigger::DirectBlockInteract teleport pads / dimension-key blocks
```

Detection is pure and bounded over a caller-supplied block accessor:

- `detect_vertical_portal` — finds the open interior around the ignite cell, verifies interior size bounds and a complete `frame_block` ring, and returns a `PortalTransition` with the minimum-corner anchor. Both x- and z-plane orientations are tried deterministically.
- `detect_horizontal_pool_portal` — 4-connected flood fill of the fluid (capped at 4,096 cells) requiring `min_size` cells and every orthogonal neighbour inside the pool or in the caller-resolved border set (`border_tags_or_blocks` entries may be `#namespace:tag`s expanded through the loaded tag table).
- `check_altitude_portal` / `check_interact_portal` — threshold and interact predicates.

Arrival:

- `compute_portal_destination` applies the exact `source_scale / target_scale` ratio to `x/z` (Overworld→Nether divides by 8; a `coordinate_scale = 4.0` modded dimension divides by 4) and clamps `y` inside the target `[min_y + 4, min_y + height - 8]` band.
- `arrival_platform` returns the deterministic platform spec the server builds when no return portal exists (frame-block pad plus matching portal interior for `VerticalFrame`; a 3×3 stone pad otherwise).
- `build_teleport_intent` produces a `TeleportIntent { target_dim, target_pos, expect_teleport_grace_ticks }`. The grace ticks feed `ac_expect_teleport(eid)` + the movement-grace window, so **a mod portal can never false-flag anti-cheat** — identical treatment to a vanilla `/execute in` transfer.

`PortalRegistry` holds the ordered rule set; `PortalRegistry::with_builtin_rules()` seeds every shipped profile.

## 4. Per-dimension environment: `DimensionEnvProfile`

Beyond `dimension_type` JSON, dimension mods change entity physics and sky rendering:

- `gravity_multiplier` — multiplies vanilla's `generic.gravity` attribute (1.21.1 default `0.08`). Reference values: Moon `0.165`, Mars `0.38`, orbit `0.05`.
- `requires_oxygen` — sealed-suit/breathing requirement (Ad Astra-style).
- `supports_underground_occlusion = has_skylight && !has_ceiling` — decides whether the 2-lane Underground Occlusion anti-xray lane applies. Ceiling dimensions (Nether-like) hide uniformly and get no benefit from the underground lane.
- `sky_effects` — the `dimension_type.effects` shader id.

`builtin_dimension_mod_profiles()` ships declarative reference profiles for `aether`, `twilightforest`, `ad_astra` (Moon/Mars/Earth orbit), `undergarden`, and `deeperdarker`; discovered JSON always overrides the reference defaults when they disagree.

## 5. Light masks beyond 64 sections

`0x27` light masks are varlong-counted `i64` bitsets. Vanilla dimensions fit one word (26/18 light sections); a 128-section modded dimension (2,048 blocks) needs two. `pack_light_mask_longs(&[bool]) -> Vec<i64>` / `unpack_light_mask_longs` implement the same little-endian word layout as `DimId::light_mask_longs` (`src/worldgen.rs`) and are verified round-trip across word boundaries, so modded heights never silently saturate.

## 6. Launcher integration (4-tier classifier)

`tools/mod_launcher.py` scans each entry's `data/` tree while discovering mods:

- `data/<ns>/dimension_type/*.json` **and** `data/<ns>/dimension/*.json` ⇒ dimension mod; its declared dimension ids, dimension_type ids, non-native `generator.type` values, and `ores*`/`chests*` tag members are emitted in the JSON report under `dimension_mods`.
- Classification rule (checked before the bytecode/profile branches): `has_custom_chunk_generator` ⇒ **Tier 3** with reason `CUSTOM_CHUNK_GENERATOR`. Pure data-driven dimension mods — no `.class` bytecode, or a declarative profile and no custom `ChunkGenerator` — stay **Tier 1**, and the `delegate` policy never starts the NeoForge runtime for them.
- A custom-generator dimension mod JAR delegates to upstream NeoForge under `delegate`, is quarantined (with the saved-world namespace guard still enforced) under `quarantine`, and refuses startup under `strict` — with the generator type listed in every report.

## 7. Invariants

1. **Zero silent skips**: every mod reports its tier and, for dimension mods, its declared dimension ids and generator verdict.
2. **Fail-closed discovery**: a generator type the engine cannot run is a reason (`CUSTOM_CHUNK_GENERATOR`), never a guess.
3. **No JVM for data-driven content**: `dimension_type` + `dimension` + `worldgen` JSON with vanilla generator types costs zero JVM and zero bounded-shim overhead.
4. **Anti-cheat honesty**: every portal transition carries teleport grace; no dimension hop may flag movement checks.
5. **Anti-xray completeness**: `ores*`/`chests*` tag members flow into the hidden set so modded ores are never trivially revealed.
