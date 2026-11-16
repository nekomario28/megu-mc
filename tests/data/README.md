# Generated 1.21.1 worldgen vectors

These files are committed test inputs, generated from the pinned Minecraft
1.21.1 server jar (SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`) through its
own classes. They are facts about that build, not code copied from it.

| File | Content | Consumer |
| --- | --- | --- |
| `random_next_int_1211.tsv` | one row per (generator, seed, bound): a hash of 256 `nextInt(bound)` results and the following `nextLong` | `generation_math::next_int_vs_vanilla_tests` |
| `end_city_mansion_1211.tsv` | `end_city` (admission, piece tree, multi-chunk placement, loot/shulker/item-frame payloads) and `mansion` (admission, layout grids, piece tree) rows; `layout` rows leave the placement columns zero | `structure_template::end_city_mansion_tests::end_city_mansion_match_1211_native_fixture`; regenerate with `tools/end_city_mansion_oracle.py --write` |
| `stronghold_chunk_-96_35_1211.tsv.gz` | seed 4242 chunk (-96,35) の全 non-`air` セル（31,721 cells）を正規化 state で。mineshaft corridor・stronghold・trial chamber が同居し、`StructurePiece.placeBlock` の postprocess が決める `oak_fence` の接続bit と、`MultifaceBlock.updateShape` が消す `glow_lichen` 2 セルを含む | `worldgen::wg_tests::structure_chunk_matches_native_capture_state_for_state`; regenerate with `tools/native_chunk_capture.py /tmp/nat-4242-stronghold/world -96 35 --out tests/data/stronghold_chunk_-96_35_1211.tsv.gz` against the same pinned build |
| `biome_probe_ties_1211.tsv` | tie-heavy climate points, the game's biome answer and an equal-distance flag | `worldgen::wg_tests::biome_tree_breaks_ties_the_way_vanilla_scans_children`; `tools/biome_tie_oracle.py` replays all rows on the actual tree |

The files came with the upstream `fix/xoroshiro-next-bound-and-bt-search-vanilla`
PR. The generator is on this branch: `tools/vanilla_data/Extract.java`, driven by
`tools/vanilla_data/gen_vanilla_data.sh` together with
`tools/vanilla_data/build_vanilla_data.py`, writes `random_next_int_1211.tsv` and
`biome_probe_ties_1211.tsv`. The other two rows name their own producer above
(`tools/end_city_mansion_oracle.py --write`, `tools/native_chunk_capture.py`).
Regenerating any of them needs the pinned jar (and Java 21 and the mapping set for
the jar-side path); that regeneration was not re-run for this document.
