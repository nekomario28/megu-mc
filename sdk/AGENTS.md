# Megu Mod SDK — AI agent authoring contract

You are an AI coding agent (Claude, Codex, Antigravity, Cursor, ...) building a
**native Megu mod**. This file is the machine-checkable contract: the exact
files, the exact bundle keys, the exact commands, and the self-heal loop that
turns `validate --json` diagnostics into a passing project without guessing.
Read this before writing any file. Author-facing prose lives in
[`docs/mod-sdk-guide.md`](../docs/mod-sdk-guide.md); the runtime authority is
[`mod-adapter-core/src/unified_bundle.rs`](../mod-adapter-core/src/unified_bundle.rs).

## 1. Non-negotiable rules

1. **Zero unsafe payloads.** No `.exe`, `.dll`, `.so`, `.dylib`, `.class`,
   `.jar`, `.bin`, `.sh`, `.bat` files in a native project. `validate` fails
   closed on them; Megu never executes a native mod corrupted this way.
2. **Zero path traversal.** No `..` path segment in ids, manifest paths, or
   file names. Symlinks must resolve inside the project root.
3. **Zero unknown bundle keys.** `megu-mod-bundle.json` is parsed fail-closed:
   a key the runtime does not know (including `$schema`!) makes the whole
   bundle invalid. Do not "fix" a validation failure by editing the schema.
4. **Zero Java boilerplate by default.** A native `.mmod` has no Gradle, no
   JDK, no Mixin, no `META-INF/neoforge.mods.toml`, no `fabric.mod.json`.
   Only add them when the user explicitly asks for Java-loader compatibility,
   and then only via `--compat-jar`.
5. **Zero borrowed assets.** Never copy Mojang or third-party textures,
   models, sounds, or language files. Generate original placeholders or
   reference vanilla asset *ids* (ids are fine; bytes are not).
6. **Determinism.** `pack --deterministic` must produce byte-identical output
   across runs. Do not inject timestamps, UUIDs, or map iteration order into
   generated JSON.
7. **Data over code.** If the behaviour can be expressed as a bundle section,
   a datapack recipe/tag, or a resource-pack asset, express it as data. Do not
   invent a Rust adapter unless the task explicitly asks for one.

## 2. Exact project layout

```
<project>/
  megu-mod.toml                    package manifest (required)
  megu-mod-bundle.json             megu-mod-bundle/1 document (required)
  pack.mcmeta                     datapack metadata (required when data/ exists)
  README.md                       what the mod does + exact commands
  data/<namespace>/recipe/*.json  vanilla-shaped recipes
  data/<namespace>/tags/item/*.json
  data/c/tags/item/**/*.json      unified #c:* common tags
  assets/<namespace>/blockstates/*.json
  assets/<namespace>/models/{block,item}/*.json
  assets/<namespace>/lang/{en_us,ja_jp}.json
  shaderpacks/<namespace>/megu-shader.toml   (shader template only)
  shaderpacks/<namespace>/final.wgsl
```

`<namespace>` is the manifest `mod.id` without any `:` prefix. Keep both
locales complete; `en_us.json` is the fallback.

## 3. `megu-mod.toml` — exact fields

```toml
schema = "megu-mod/1"          # required, exact

[mod]
id = "ruby_starter"            # required; ^[a-z][a-z0-9_.-]{0,63}$; equals the bundle mod_id
name = "Ruby Starter"          # required, non-empty
version = "0.1.0"              # required, SemVer MAJOR.MINOR.PATCH
mc_version_range = "[1.21.1,)" # required; must equal the bundle mc_version_range
authors = ["Example Author"]   # required, non-empty array
license = "MIT"                # required, non-empty
description = "..."            # required, non-empty

[bundle]
file = "megu-mod-bundle.json"  # required, exact
```

`megu-mod.toml` is read by `tools/megu_mod_dev.py`; the server itself consumes
`megu-mod-bundle.json`.

## 4. `megu-mod-bundle.json` — exact contract

Top-level keys (nothing else is accepted):

| key | required | shape |
| --- | --- | --- |
| `schema` | yes | exactly `"megu-mod-bundle/1"` |
| `mod_id` | yes | resource location (`ruby_starter` or `ruby_starter:core`) |
| `mc_version_range` | yes | Maven `[1.21.1,)` or SemVer `>=1.21.1 <1.22` |
| `portals` | yes (may be `[]`) | array, max 1024 |
| `progression_barriers` | yes (may be `[]`) | array, max 1024 |
| `biome_regions` | yes (may be `[]`) | array, max 1024 |
| `creatures` | yes (may be `[]`) | array, max 1024 |
| `custom_channels` | yes (may be `[]`) | array of resource locations |
| `spells` | no | array, max 1024 |
| `custom_statuses` | no | array, max 1024 |
| `rituals` | no | array, max 1024 |

Id rules: namespaces `[a-z0-9_.-]+`, paths `[a-z0-9_./-]+`, one optional `:`,
no `..` segment. Item lists that accept tags use a leading `#`.

Variants (each selected by a `kind` discriminator; an unknown value is
rejected, never defaulted):

- `portals[].trigger.kind`: `vertical_frame` | `horizontal_pool` |
  `altitude_threshold` | `direct_block_interact`
- `creatures[].movement.kind`: `land` | `amphibious` | `aquatic` |
  `gliding_flight` | `wall_climb`
- `spells[].cast.kind`: `instant` | `cast_time` | `channeled`
- `spells[].steps[].form.kind`: `self_cast` | `touch` | `projectile` |
  `raycast_beam` | `aoe_zone` | `chain` | `orbit` | `summon`
- `spells[].steps[].effects[].kind`: `damage` | `heal` | `shield` |
  `apply_status` | `knockback` | `blink` | `conjure_light` |
  `break_block` | `emit_vfx`

Bounds the validator enforces (mirroring the Rust parser): spells have
`1..=16` steps and `<=8` effects per step; `aoe_zone.radius` in `(0, 32]`;
`chain.max_hops` in `1..=8`; `orbit.count` in `1..=32`;
`summon.max_active` in `1..=16`; ritual offsets in `-16..=16`, `<=256`
pattern blocks, `<=64` reagents with counts `>= 1`; durations
`1..=72000` ticks; `custom_statuses` `<=16` attribute modifiers; duplicate
spell/status/ritual ids or duplicate ritual offsets are errors.

The full annotated schema (descriptions, patterns, bounds, defaults) is
[`sdk/schemas/megu-mod-bundle-v1.schema.json`](schemas/megu-mod-bundle-v1.schema.json).

## 5. Commands (copy them verbatim)

```sh
# scaffold: starter | machine | creature | shader
python3 tools/megu_mod_dev.py init ruby_starter --out ruby-starter --template starter

# validate; --json is the machine contract you must consume
python3 tools/megu_mod_dev.py validate ruby-starter --json

# deterministic package + sha256/sha512 for Modrinth / ModSync
python3 tools/megu_mod_dev.py pack ruby-starter --deterministic

# optional Java-loader compatibility (only when asked)
python3 tools/megu_mod_dev.py pack ruby-starter --compat-jar --out dist/ruby-starter-0.1.0.jar

# one deterministic sync cycle into a server mods/ directory
python3 tools/megu_mod_dev.py watch ruby-starter --mods-dir ./mods --once

# environment check
python3 tools/megu_mod_dev.py doctor --json
```

`validate --json` prints exactly:

```json
{
  "ok": false,
  "mod_id": "ruby_starter",
  "diagnostics": [
    {
      "code": "bundle.unknown_key",
      "path": "megu-mod-bundle.json#/$schema",
      "message": "unknown key '$schema' (rejected by DeclarativeModBundle::from_json)",
      "fix_hint": "remove '$schema'; the runtime accepts only [...]",
      "severity": "error"
    }
  ]
}
```

Exit status: `0` no errors (warnings allowed), `1` errors, `2` usage error
(invalid mod id, non-empty target without `--force`, missing project).

## 6. Self-heal loop (do this instead of guessing)

1. Write the project.
2. Run `validate <dir> --json`.
3. If `ok` is true, run `pack <dir> --deterministic` and stop.
4. Otherwise, iterate **only** over `diagnostics` entries with
   `severity == "error"`. Open the file named by `path` (strip the `#/...`
   JSON pointer to get the file name, then follow the pointer), apply the
   smallest edit that satisfies `fix_hint`, and re-run the same command.
5. Never widen the schema, never delete a required key, never suppress a
   diagnostic by adding an ignore flag. If a diagnostic genuinely contradicts
   the user's requirement, stop and report it instead of guessing.
6. Cap the loop at ~10 iterations; if it does not converge, print the last
   JSON report and ask the user.

## 7. What each primitive covers

| Need | Where it lives | Declarative? |
| --- | --- | --- |
| Tameable/breedable mobs, rideable, shoulder perch, 5 movement profiles | bundle `creatures` → `mod-adapter-core/src/creature_primitives.rs` | yes |
| Large / multi-phase "colossal" bosses: server phases + hitboxes | `mod-adapter-core/src/world_extensions.rs` (`BossEncounterState`, `SubPartHitbox`) | Rust adapter |
| Colossal client rigs (winged dragon / sea serpent) | `client-port/src/entity_rig.rs` (`ColossalRigKind`, `build_colossal_rig_mesh`) | Rust adapter |
| Portals (frame, fluid pool, altitude, block interact) | bundle `portals` → `dimension_mod.rs` | yes |
| Advancement-gated dimensions/biomes, periodic damage | bundle `progression_barriers` → `world_extensions.rs` | yes |
| Weighted biome replacement | bundle `biome_regions` → `BiomeRegionBlender` | yes |
| Spells, statuses, rituals | bundle `spells` / `custom_statuses` / `rituals` → `magic_primitives.rs` | yes |
| Item/fluid/energy transfer between blocks | `mod-adapter-core/src/capability_bus.rs` (`BlockCapabilityBus`, `SimpleSlotInventory`, `SimpleFluidTank`, `CookingPotItemAdapter`) | Rust adapter |
| Create-style kinetics, belts, addon profiles | `create-adapter/src/factory.rs`, `create-adapter/src/addon_profile.rs` | Rust adapter |
| Farmer's Delight-style cooking/cutting/fermenting | `mod-adapter-core/src/culinary_adapter.rs` | auto-discovered data |
| `#c:` / `#forge:` / `#neoforge:` aliasing, nested tags | `mod-adapter-core/src/unified_tags.rs` (`UnifiedTagIndex`) | data + index |
| Recipes, tags, loot, blockstates, models, lang | `data/` + `assets/` | yes |
| WGSL shader passes | `shaderpacks/<ns>/megu-shader.toml` + `.wgsl` | data |

There is **no** `blocks` / `items` / `events` / `dimensions` bundle key. A
bundle may *reference* such ids; registering the underlying block/item still
needs the native registry path or a datapack. If the user's mod needs new
block/item registration, say so explicitly — do not smuggle it into the
bundle.

## 8. Definition of done

```sh
python3 tools/megu_mod_dev.py validate <project> --json   # ok: true, zero errors
python3 tools/megu_mod_dev.py pack <project> --deterministic
python3 tests/megu_mod_dev_test.py                        # from the Megu checkout
python3 tools/docs_link_audit.py                          # if you touched docs/
```

Report: files created, `sha256`, `sha512`, validation summary, and anything
the data-only path cannot express (new blocks/items, custom render passes,
Java-only APIs).
