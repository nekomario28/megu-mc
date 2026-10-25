# Pinned biome carver lists

Target authority is Minecraft 1.21.1 / protocol 767, server SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. No dependency, server/world startup,
GPU or donor execution is introduced.

## Root cause

The own-jar biome JSON writes `carvers` as an object with a generation step:

```json
"carvers": {"air": ["minecraft:cave", "..."]}
```

and a one-entry step as a bare string (`"air": "minecraft:nether_cave"`).
Converted packs wrote a bare list. `load_creg` called `jarr(j, "carvers")`,
which is `None` for every own-jar biome, so **all carver lists were silently
empty** and `apply_carvers` never applied a configured carver on the migrated
data. The first own-jar run of `carver_configs_parse_the_1_21_1_field_shapes`
failed exactly at `forest carvers`.

`carver_list` now accepts the 1.21.1 object (`air` as an array **or** a single
string) and the converted bare list/string. The order is preserved because the
carver index is the seed offset. `nether_cave` is retained in the nether biome
lists, but its configured carver is still unsupported; that is reported and the
overworld behaviour does not depend on it.

The remaining five feature tests failed for a different own-layout reason:
tests read `worldgen/feature/...`, while the jar uses
`worldgen/configured_feature/...`. `pack_cf` now tries `configured_feature` first
and falls back to `feature`, so the dripstone/root tests read the pack that is
actually installed.

## Reference and scope

`vanilla.json` is produced by `tools/biome_carver_oracle.py`:

- the authenticated jar's own datapack is loaded by `RegistryDataLoader` through
  the real codecs;
- every `Biome.getGenerationSettings().getCarvers(GenerationStep.Carving.AIR)`
  holder is read in order and its `ResourceKey.location()` recorded.

The fixture has **64 biomes, 58 non-empty AIR lists**. The Rust test compares
every list, including order; forest is the three-carver list the carver seed
index depends on. No server/world, `Unsafe` allocation or constructor bypass is
used. Spawners and `spawn_costs` are not read by this oracle and are not
implemented by the server.

## Verification

| Gate | Result |
| --- | --- |
| Native carver replay, twice | 64 biomes exactly equal; 58 non-empty lists |
| Offline oracle guards | malformed/extra/reordered/stale/wrong-jar cases rejected; output preserved on wrong jar |
| Focused Rust `carver` | 4 passed (config parse, native list/order, converted list/string, bundled configs) |
| Selected canonical mutations | 4 proved; 0 not-a-gate/parked/skipped |
| fmt / all-target warnings-denied clippy | Passed |
| Layers / debt / wire | 19 modules, 44 edges, 0 named errors; 0 over limit; container and gameplay wire PASS |
| Full serial release | **232 passed / 0 failed / 1 ignored**, exit 0 |

The previous slice was 213/17/1. **Seventeen failures now pass, two new tests
were added, and none was added to the red set:** twelve were carver-dependent
(`carver_configs_parse…`, feature-population/lazy captures, material rules,
`not_underwater…`, perf probe, surface density, ore vein, biome cache, vertical
gradient), and five were the own-layout feature fixtures (dripstone cluster/large,
root system). The suite is
green locally; it is **not** a whole-world or native-world acceptance claim.
Carver cell geometry still rests on the bounded `CarverProbe` history, not a new
whole-chunk comparison.

## Independent review and corrections

A static review of the slice found five issues; all are fixed here:

- six End biomes have `"carvers": {}` and were not inserted at all, while the
  test treated a missing entry as an empty list. `carver_list` now returns an
  empty AIR list for an object without `air`, and the test asserts presence;
- `pack_cf` tried `configured_feature` first while the production merge gives
  `feature/` precedence. It now follows the loader's order;
- the converted bare-list/string branch had no fixture or test; it now has
  `converted_carver_lists_still_parse` and a mutation proof;
- the status page still said the suite was not green; corrected;
- the receipt was missing; this directory now carries `run.json` and logs.

The review did not execute Cargo, Java or a world. Its remaining "sound" checks
were static. The fixed gate was rerun serially; see `suite.txt`.

## Limits and failed attempts

- Only the AIR carver list loading is closed. `spawners` / `spawn_costs`,
  `nether_cave` execution, biome spawn runtime, full surface context, arbitrary
  seeds/chunks, forest stone/deepslate attribution and same-stage whole-world
  comparisons remain outside this slice.
- The fixture is data/loader evidence, not a cell-level carver differential.
- Full `tests/verify.sh`, the whole mutation campaign and hosted CI were not run.
  Hosted PR checks remain unstarted because of account billing, not code results.
- No push, main merge or release was performed.

See [the closed rationale](../../../specs/done/biome-carvers/README.md) and
`run.json`.
