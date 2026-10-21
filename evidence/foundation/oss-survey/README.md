# OSS survey — what can be imported into mc-spine (MIT)

mc-spine is MIT. That rules out GPL/AGPL code (Pumpkin, SteelMC, Paper, Amidst)
and unlicensed repos. This file records what the survey found, what is
*importable*, and what each candidate is actually good for. Reproduce with the
queries in §6.

Verdict vocabulary: **import** = license-compatible and worth copying code
from; **cross-check** = use as a second implementation to diff behaviour
against; **ideas** = read only, do not copy; **no** = license forbids it.

---

## 1. Tier 1 — importable (MIT / Apache-2.0 / BSD / CC0 / Unlicense)

| repo | license | lang | what it covers | verdict |
|---|---|---|---|---|
| `Soju06/hyperchunk` | MIT | C11 | **complete overworld pipeline**: noise/DF/SIMD eval, surface rules, carvers, aquifer, features (tree/lush/ring), structures, lighting, spawn, NBT+region, scheduler | **import + cross-check** |
| `Cubitect/cubiomes` | MIT | C | exact biome generation, structure positions, spawn, seed math (no carvers) | cross-check |
| `MCHPR/MCHPRS` | MIT | Rust | redstone + tick semantics, multi-threaded world | import (redstone) |
| `bedrock-crustaceans/chorus` | Apache-2.0 | Rust | Bedrock worldgen incl. aquifer, lighting, redstone, 1800 files | ideas/import (pipeline shape) |
| `jacobsjo/mc-datapack-map` | MIT | TS | datapack-driven DF/noise evaluation, worldgen datapack handling | cross-check |
| `PrismarineJS/minecraft-data` | (see repo) | JS | block states, tags, registries for every version | import (data) |
| `PrismarineJS/node-minecraft-protocol` | BSD-3 | JS | packet schemas per version | cross-check |
| `valence-rs/valence` | MIT | Rust | server framework, protocol, chunk encoding | import (plumbing) |
| `azalea-rs/azalea` | MIT | Rust | client/bot crates, world + physics | cross-check |
| `ferrumc-rs/ferrumc` | MIT | Rust | full server reimplementation (no worldgen yet) | ideas |
| `Quozul/PicoLimbo` | MIT | Rust | minimal server, login/config phase | import (protocol) |
| `PistonDevelopers/hematite_server` | MIT | Rust | old server clone | ideas |
| `Noeli14/MCppServer` | MIT | C++ | server | ideas |
| `RobertLeahy/MCPP` | Unlicense | C++ | server (2014) | ideas |
| `DuncanRuns/ravines` | CC0 | C | tiny ravine/canyon simulator (needs cubiomes) | cross-check |
| `LudicrousMC/ludigen` | MIT | C | terrain generation at speed, noise kernels | ideas |
| `jaquadro/NBTExplorer` | MIT | C# | NBT | import (tooling) |
| `mcedit/mcedit` | ISC | Python | region/world editing | ideas |
| `Quozul/minecraft_world_trimmer` | MIT | Rust | region file surgery | import (tooling) |

## 2. Tier 2 — reference only (license forbids copying)

| repo | license | note |
|---|---|---|
| `Pumpkin-MC/Pumpkin` | GPL-3.0 | carver/aquifer exist but are welded to 26.3 data; **no code** |
| `Steel-Foundation/SteelMC` | AGPL-3.0 | `steel-worldgen/src/noise/aquifer.rs` documents the same 16×12×16 grid; **no code** |
| `toolbox4minecraft/amidst` | GPL-3.0 | biome/structure layers |
| `Earthcomputer/fluffy-parakeet` | **none** | Rust worldgen; without a license we may not copy |
| `BlueDragonMC/SteelWorldGen` | NOASSERTION | ditto |

---

## 3. hyperchunk — the one that matters for the carver blocker

`Soju06/hyperchunk` is MIT, C11, and claims bit-exact overworld generation for
MC 26.2 with region-level golden tests. Two things make it valuable beyond the
claim:

**(a) `openspec/` is a bytecode reconstruction, not a summary.** Files like
`openspec/changes/archive/2026-07-30-task8-carvers/A5-carvingcontext-aquifer.md`
state which class/method each rule came from and which questions were open and
how they were resolved. That is exactly the discipline this repo uses
(`evidence/gameplay/carvers/README.md`), so it can be cited directly.

**(b) Its `core/src/aquifer.c` is an independent port of the same bytecode we
ported — and it matches ours branch for branch.** Checked line by line on
2026-10-21:

| element | hyperchunk | mc-spine |
|---|---|---|
| `similarity(a,b)` | `1.0 - (b-a)/25.0` with `a<=b` always | `1.0 - (d1-d0).abs()/25.0` |
| 13 surface offsets | `SURF_OFFS[13]`, `{0,0}` first | `AQ_OFF`, identical order |
| 12-cell scan | `dx 0..1 / dy -1..1 / dz 0..1`, cell base `(x-5, y+1, z-5)` | same |
| sample position | `gx*16+nInt(10), gy*12+nInt(9), gz*16+nInt(10)` | same |
| nearest 4 | ties broken by `>=` (insert at front) | ties by `>` (keeps earlier) — only differs on exact ties |
| early return | `sim12 <= 0 → fs_at(fs1, y)` | same |
| water-over-lava rule | `bs == WATER && picker(y-1).at(y-1) == LAVA → bs` | same |
| pressure tests | `sim12*calc(f1,f2)`, `sim12*sim13*calc(f1,f3)`, `sim12*sim23*calc(f2,f3)` | same |
| `calc` short-circuits | states-at-y lava/water → 2.0; `|Δlevel| == 0` → 0 | same |
| level | `min(dampener, spread cell)`, `clamp`+`map(-0.3,0.8)`/`(-0.8,0.4)` | same |
| kind | `level <= -10 && level != sentinel && type != lava` → `|lavaNoise| > 0.3` | same |
| `minEst` source | PSL DF at `((x>>2)<<2, 0, (z>>2)<<2)`, `floor`, memoised | same |
| `skipSamplingAboveY` | `(gridY(maxPSL+8+12)+1)*12 + 10` | same formula |
| carve-time lava floor | `y <= resolveY(lava_level)` (= −56 for `above_bottom 8`), bypasses the aquifer | same |

So **two independent reconstructions agree on every branch**. That does not
prove correctness for 1.21.1 (both could be wrong in the same way), but it does
mean the carver residual is unlikely to be inside the aquifer logic. hyperchunk
also settles the carve-vs-fill mode question we had: the barrier noise is
evaluated with the caller's context (fill = NoiseChunk/BLOCK, carve =
SinglePoint/SP), and that is value-neutral for shipped data because `barrier`
is a plain noise node.

What to take from hyperchunk next, in order:
1. **Golden harness shape**: they normalise save-time fields and sha256 full
   chunk payloads, and run a REPLAY scheduler to make the comparison
   order-independent. Our `tools/carver_diff.py` counts cells; a payload hash
   gate would catch palette/heightmap/postprocess drift we cannot see.
2. **Lighting stages** (`gen_light_stages.c`, `hc_light.*`) — the part of our
   port that has no parity harness yet.
3. **Feature RNG discipline** (`features_rng.c`) and the tree/lush feature
   implementations, when the carver gate closes.
4. **SIMD DF evaluation** (`df_simd_avx2/512.c`, `df_isa.c`) if generation speed
   ever matters for the exit criteria.

Caveats to keep in mind: it targets **26.2** (data, tags and a few algorithms
differ from 1.21.1 — do not copy JSON or tag lists), it is one author and three
weeks old with no external verification, and its light/structure stages are the
least documented (the openspec archive covers noise/surface/carvers/features).

---

## 4. What this survey changes

- The carver residual should stop being chased inside `aq_state`: an MIT,
   independently written, golden-tested implementation of the same bytecode
   agrees with ours on every branch (§3b). The remaining suspects are the
   *plumbing* (which cells are queried, and what is done with the answer) and
   the fixture.
- Redstone has a credible MIT donor (`MCHPRS`) before we ever need it.
- Lighting and structures have a donor (`hyperchunk`) whose docs are already in
   the bytecode-reconstruction style this repo uses.
- Nothing in Tier 1 is a drop-in replacement for our world model; all of it is
   either a cross-check or a component. The earlier strategy conclusion
  (`L1 は 0 か 1`) is unchanged.

## 5. Non-goals of this survey

- No candidate was executed or vendored here. Reading code and diffing
  behaviour is fine; copying requires the license header to travel with it.
- Version coverage was not audited for Tier 1 entries beyond what their README
  states.

## 6. How the search was run

`gh search repos` with ~60 queries (worldgen/carver/aquifer/chunk/noise/anvil/
protocol/redstone/lighting/NBT/region/structure/biome/data + language filters),
plus `gh search code` for identifiers that only appear in ports:
`ChunkNoiseSampler`, `NoiseBasedAquifer`, `aquifer`, `carver`, `find_top_surface`.
Every hit's license was read from the GitHub API; anything without an SPDX
license was moved to Tier 2 as "ideas only".
