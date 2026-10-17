# Share the biome leaf across the material rule (M3)

## What was expensive

The deep material rule reads the biome once per **block** (`SC::leaf_at` -> `Gen::biome_quart`,
a B-tree search over the multi-noise biome tree). `deep_state_i` made a fresh `SC` each call, so
its per-SC `leafq` cache never survived a block. A representative 15 s moving-view load measured:

| counter | count |
|---|---|
| `deep_state_i` calls | 3.36 M |
| `biome_quart` calls | 4.21 M |
| distinct quarts in the working set | about 0.12 M |

The same quart was searched about 34 times, once per block it contains (a quart is 4x4x4 blocks),
so the material rule paid a full biome search for almost every block.

## The change

`Gen` gained `bioq: HashMap<u64, u16>` / `bioq_ord: VecDeque<u64>` (bounded `1 << 18`), and
`leaf_at` now caches the quart -> leaf there. `biome_quart(qx, qy, qz, None)` is a pure function of
the quart (its climate is already cached by column and the tree does not change after load), so a
shared cache returns exactly what the per-SC cache did. The look-up is byte-identical, not an
interpolation or an approximation, and the worldgen output is unchanged (see below).

## Result

`biome_quart` calls fell **4.21 M -> 0.32 M** (about 13x). The representative load (15 s, seed 0,
view 4, same client, fixed `tools/load_compare.sh` measuring the freshly built binary):

| | CPU s | RSS MiB | threads |
|---|---|---|---|
| vanilla | 16.09 (14.76 / 15.49 / 16.98 / 17.14) | 1134 | 64 |
| mc-spine | **10.16** (9.86 / 10.29 / 10.25 / 10.22) | 140 | 3 |

Ratio **x0.63** — mc-spine now does about **1.58x less work** than vanilla, with about 1/8 the
resident memory and 1/21 the threads. This clears the M3 target (which needs at least x1.10).

The isolated feature pass (`feat_populate_perf`, `populate9`) also dropped, and the parallel test
suite went from about 109 s to about 44 s for three eight-thread runs.

## Parity

- 137 tests pass / 1 ignored, including `feature_population_output_matches_the_pre_refactor_capture`
  (the frozen population digest) — the worldgen output is unchanged.
- `the_shared_biome_cache_returns_the_same_leaf_for_the_same_quart` pins that a repeat query returns
  the first result and agrees with a direct `biome_quart` lookup.
- Mutation: storing `l.wrapping_add(1)` in the cache makes that test red (`tools/mutation_check.sh`,
  44 proved / 0 skipped).

## Limits

- The load is a CLI protocol client, not a rendered client; on-screen parity is the excluded GUI path.
- The quart cache is bounded at 262144 entries (about 15 MiB); a session that streams far more
  unique quarts evicts in FIFO order, which only costs a recomputed search, never a wrong leaf.
