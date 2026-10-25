# Generated 1.21.1 worldgen vectors

These files are committed test inputs, generated from the pinned Minecraft
1.21.1 server jar (SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`) through its
own classes. They are facts about that build, not code copied from it.

| File | Content | Consumer |
| --- | --- | --- |
| `random_next_int_1211.tsv` | one row per (generator, seed, bound): a hash of 256 `nextInt(bound)` results and the following `nextLong` | `generation_math::next_int_vs_vanilla_tests` |
| `biome_probe_ties_1211.tsv` | tie-heavy climate points, the game's biome answer and an equal-distance flag | `worldgen::wg_tests::biome_tree_breaks_ties_the_way_vanilla_scans_children` |

The files came with the upstream `fix/xoroshiro-next-bound-and-bt-search-vanilla`
PR. The generator used by that PR (`tools/vanilla_data/Extract.java`) is not
present on this branch; the vectors are therefore inputs, not a claim that this
branch can regenerate them. Regenerating would require the pinned jar and the
mapping set the PR used.
