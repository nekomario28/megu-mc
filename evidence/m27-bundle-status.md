# M27 — `synthesize_bundle` populates what parsed JAR data provably declares

## Scope

`mod-adapter-core/src/auto_adapter.rs` only (`synthesize_bundle` + in-file
tests). No other file touched; changes left uncommitted.

## Investigation

### `DeclarativeModBundle` fields (`mod-adapter-core/src/unified_bundle.rs:112`)

The `megu-mod-bundle/1` schema is a **closed gameplay-mechanics contract**:

- `mod_id`, `mc_version_range`
- `portals: Vec<PortalRule>` — trigger kinds `vertical_frame`,
  `horizontal_pool`, `altitude_threshold`, `direct_block_interact`
- `progression_barriers: Vec<ProgressionRule>` — needs non-empty
  `target_biomes` **and** non-empty `required_advancement` ids
- `biome_regions: Vec<BiomeRegion>` — needs `dimension` +
  `vanilla_biome -> mod_biome` override map + `weight`
- `creatures: Vec<CreatureSpec>` — entity type + health/tame/breed/movement
- `custom_channels: Vec<String>` — `CustomPacketPayload` channel ids
- `spells`, `custom_statuses`, `rituals` — magic-primitive definitions

There are **no declared-entity fields** (no blocks/items/recipes/dimensions
sections), and `from_json` rejects unknown keys — so blocks/items/recipes
can never ride inside the bundle document. Downstream consumers
(`UnifiedModRuntime::register_bundle`, `live_mod_bridge::install_bundle`,
`write_adaptation_outputs` → `<mod_id>.bundle.json` → strict re-parse in
`scan_bundles_dir`) see exactly the same `DeclarativeModBundle` type; the
manifest-stub role is preserved.

### What `data` (`ModDataBundle`) provably holds for real 1.21.1 JARs

`extract_mod_data` lifts, for ANY namespace, under bounded reads:

- `recipes: Vec<ParsedModRecipe>`, `tags`, `loot_tables` (full `JsonValue`)
- `dimensions: DimensionModData` — `dimension_type` descriptors,
  `dimension` entries (`id`, `type_id`, `generator_kind`), worldgen id
  lists, ore/chest hidden-tag blocks
- `data_maps` (NeoForge 1.21.1), `biome_modifiers` (NeoForge 1.21.1)
- `blockstates` (property tables), `models`, `items` (declared asset ids)
- `namespaces`, `rejected_members` (fail-closed diagnostics)

### Per-section synthesis verdict

| Bundle section | Synthesis source | Verdict |
|---|---|---|
| `portals` | `builtin_dimension_mod_profiles()` portal rules, gated by parsed `dimension/` ids | **Synthesized** — the crate's curated reference data for the five well-known dimension mods (aether, twilightforest, ad_astra, undergarden, deeperdarker), the same source `DimensionModRuntimeBridge` trusts for env profiles |
| `progression_barriers` | needs advancement ids; `data/advancement` is not an extracted family | empty — genuinely unknown |
| `biome_regions` | needs vanilla→mod biome override map; worldgen biome ids do not declare substitutions | empty — genuinely unknown |
| `creatures` | no entity registry data is parsed (entity tags are generic tag values, not `CreatureSpec` fields) | empty — genuinely unknown |
| `custom_channels` | payload channels are registered in code, not in `data/` | empty — genuinely unknown |
| `spells` / `custom_statuses` / `rituals` | no magic-primitive data family exists | empty — genuinely unknown |

Blocks/items/recipes/dimensions/data-maps/biome-modifiers have no bundle
fields by design; they already flow verbatim through
`SynthesizedBundle.data` → `JarAdaptationResult.data_bundle` → the
`megu-jar-adaptation/1` report's `data`/`data_bundle` sections.

## Change

- `synthesize_bundle` now populates `portals` for the synthesized (non-
  embedded) path: for each `builtin_dimension_mod_profiles()` entry, if
  the JAR's parsed `data.dimensions.dimensions` contains one of the
  profile's dimension ids, the profile's portal rules whose `target_dim`
  the JAR also declares are cloned into the bundle. Gating is by parsed
  dimension evidence only — no mod-id hardcoding added; the same gate
  `load_dimension_mod_entries` applies to env profiles.
- A `native_surface` audit note records the synthesized portal count.
- Doc comment on `synthesize_bundle` now states the per-section contract.

## Tests

- New `synthesized_bundle_gains_profile_portals_for_dimension_mods`:
  realistic 1.21.1 Aether `dimension_type`/`dimension` JAR fixture (no
  embedded bundle, no bytecode) → `NativeBundle`; asserts the bundle
  carries `aether:the_aether_portal` → `aether:the_aether`, all other
  sections stay empty, the document round-trips strict
  `DeclarativeModBundle::from_json`, the parsed dimension id flows through
  `data_bundle`, and the audit note records the portal synthesis.
- `data_driven_dimension_mod_is_native` extended: `dimmod:mydim` matches
  no builtin profile → synthesized bundle keeps empty portals.

## Verification

- `cargo check --offline --locked -j2` (mod-adapter-core): PASS.
- `cargo test --release --offline -j2 --lib` (mod-adapter-core): 148
  passed, 0 failed — includes all 28 `auto_adapter` tests.
- `cargo test --release --offline -j2 bundle`: PASS across all targets.
- `cargo test --release --offline -j2 --test multi_mod_archetypes
  --test auto_adapt_cli --test unified_bundle_tests`: PASS (the pinned
  "synthesized bundle stays section-empty" archetype contract still
  holds — `hearthcraft` ships no profile dimension).

## Boundaries

- Only `mod-adapter-core/src/auto_adapter.rs` modified (the task's
  `src/auto_adapter.rs` path resolves to this file; `src/` has no such
  module). Concurrent `src/*.rs` edits by other agents untouched.
- Portal rules come from existing crate reference profiles, not new
  hardcoded dictionaries; no gameplay semantics were fabricated — every
  other section remains empty because parsed data provably does not
  declare it.
- Changes uncommitted; no git commit or push performed.
