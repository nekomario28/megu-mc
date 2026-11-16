# What the residuals actually are: an unmeasured brightness half, writes the save does not keep, and a spurious fossil beard

Every claim on this page comes from a native capture bound to this tree's own sources and from the
port binary built from the same tree. No conclusion on this page is adopted from
`mushroom-brightness-and-basalt-columns-20261116.md`: two of that page's claims are **falsified
below** (its brightness attribution, and its `count_on_every_layer` layer arithmetic), and one of
its instruments was found to be logging almost nothing (`FCELL`).

**No parity change is landed.** Both remaining residuals turn out to be *not* in the code the
earlier lanes were rewriting: the mushroom cells are a `canSurvive` half that is now measured to be
0 on both sides plus a block-shape update the port does not model, and the `(-25,22)` delta-layer
difference is a beardifier input that the reference does not have. Changing either alone makes a
fixture worse, and both numbers are recorded below.

## Base state

| | |
|---|---|
| base commit | `dfe52eb` (`e7ead8e` + cherry-picks `89ac2124`, `fc9dfdf`) |
| branch | `t3/c5e6c95f` |
| dirty at measurement | `src/feature.rs`, `src/generation_context.rs`, `src/generation_contract_tests.rs`, `src/generation_providers.rs`, `src/structure_pipeline.rs`, `tools/carve-agent/{Agent,CarveLog,native_fixture}.sh|java`, `tools/mutation_check.sh` |
| result commit | the commit that adds this page (see `git log -- evidence/gameplay/feature-parity/light-and-lost-writes-20261116.md`) |
| native jar | `mc-native-overworld-20261111/server.jar` sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| mappings | `/var/tmp/mcm/server.txt` (the same file `evidence/gameplay/feature-parity/native-oracle.md` cites) |

Port binary at every measurement below: the `dev` profile build of this tree,
`sha256 18ed8a85224c705666eb45226dbfc3dbcd3d7b69ce1b81244b724f8ce48aaeba` (`target/debug/mc-spine`,
before the last comment-only edits; every number is reproducible from the commands shown).

## Instruments this lane added (and one repaired)

1. **`NCEL`** (`CarveLog.celLayer`, `Agent.instrumentCelLayers`, `-Dcelchunk=<cx>,<cz>`): the
   reference's answer for every `CountOnEveryLayerPlacement.getY` sample -
   `NCEL <x> <z> top=<top> layer=<l> y=<y>` with `y=2147483647` for "no such layer". Hooked on
   `eim.a(Leiw;IIII)I`, both `IRETURN` sites. This is the ground truth the previous lane's
   "four of 200" claim needed and did not have.
2. **`FCELL` repaired.** `instrumentWrites` hooked only `arb.a(jd,dtc,II)Z`. A whole populated
   Nether fixture logged **12** write lines that way (measured 2026-11-16), which reads as
   "features wrote almost nothing" rather than "the hook missed the call". `WorldGenLevel`'s
   feature writes reach `arb` through the same method, so the 12 lines were writes from a caller
   that happens to use the four-argument form. With the hook widened to both descriptors the same
   fixture logs **58 606** lines. (`arb` itself declares only `(jd,dtc,II)Z`; the one-argument form
   is admitted for `callback == "featureCell"` and matches nothing today - it is there so the next
   `arb`-shaped class is not silently half-traced.)
3. **`MC_FIXTURE_JAVA_EXTRA`** (`tools/carve-agent/native_fixture.sh`): whitespace-separated `-D`
   hooks for one measurement, recorded in the capture's own `java-extra.txt`. A later `-D` wins, so
   this also overrides the harness's default `-Dfeaturebox`.
4. **`CEL_COLUMN_TRACE=<x>,<z>` + `FC::get_src`** (`src/feature.rs`, `src/generation_context.rs`):
   the live feature-facing column, one `CCELL <x> <y> <z> <state> <layer>` line per cell, where
   `layer` is `wr` / `rc` / `ovr` / `terrain`. A boundary list says *which* y differs; this says
   *which layer answered*, which is what separated "the port's terrain is different" from "the
   port's terrain read is stale".
5. **`BEARD_TRACE` / `BEARDPIECE`** (`src/structure_pipeline.rs`): the beardifier input for one
   chunk (`BEARD <cx>,<cz> rigids= junctions= interesting=`) and the start each rigid box came from
   (`BEARDPIECE <cx>,<cz> <structure> start=<sx>,<sz> bb=[..]`).

Contract test and mutation for (4): `cel_column_trace_tags_the_layer_that_answered` and
`check "a traced column names the layer that answered it"`; the mutation is proved
(`MUTATION_FILTER=cel_column_trace_tags_the_layer_that_answered … 1 proved, 0 not a gate`).

## 1. `getRawBrightness(pos, 0)` inside `MushroomBlock.canSurvive` is **0**, in every dimension

`-Dmushroomchunk=<cx>,<cz>` logs the int `dla.a(dtc,dcz,jd)` reads from `dcz.b(pos,0)` (the hook
preserves the value: `ISTORE 200` … `ILOAD 200`). Two captures, both `MC_FIXTURE_STAGE=structures`,
seed 4242, frozen, harness defaults otherwise:

| capture | reads | values |
|---|---|---|
| `nat-nf-str` (Nether, `(-25,19)+3x3`, 81 decorated chunks) | **464** | **464 x `0`** |
| `nat-taiga-mlight` (Overworld, `(4,-32)+3x3`, village taiga) | **403** | **401 x `0`**, 1 x `1`, 1 x `2` |

Commands:

```sh
MC_FIXTURE_STAGE=structures MC_SPINE_SEED=4242 \
MC_FIXTURE_JAVA_EXTRA='-Dcelchunk=-25,22 -Dmushroomchunk=-25,19 -Dfeaturebox=-400,0,304,-368,127,383' \
  bash tools/carve-agent/native_fixture.sh <dir> -25 19 3 frozen        # the Nether capture
MC_FIXTURE_STAGE=structures MC_SPINE_SEED=4242 \
MC_FIXTURE_JAVA_EXTRA='-Dmushroomchunk=6,-32' \
  bash tools/carve-agent/native_fixture.sh <dir> 4 -32 3 frozen         # the Overworld capture
```

Consequences, which are the opposite of the withdrawn attribution:

* the four Nether residual cells were **not** refused for being lit. All four read `0`, and so do
  the 24 cells both sides place:
  `grep "MLIGHT -399 39 308\|MLIGHT -396 38 304\|MLIGHT -395 38 307\|MLIGHT -400 39 308"` → `light=0`
  on all four;
* the Overworld taiga cell `(99,68,-502)` reads `0` too, so the port's
  `uninitialized_raw_brightness` = 15 for a sky dimension is **wrong** (the port refuses that
  mushroom; the reference places it);
* the two non-zero Overworld reads are `1` and `2`. Under `canSurvive`'s `< 13` test they are the
  same case as `0`, so they do not need a model - but they do prove the engine is not uniformly 0,
  i.e. the value is "whatever the running server had", not a dimension default.

## 2. The residual cells are writes the saved chunk does not keep

The reference places a mushroom at **every** residual cell. From the same Nether capture
(`state=2089` is `brown_mushroom`, `state=2090` is `red_mushroom` from the pinned state table):

| | count |
|---|---|
| `FCELL … state=2089` in chunk `(-25,19)` | 14 |
| `FCELL … state=2090` in chunk `(-25,19)` | 14 |
| mushrooms in the **saved** chunk `(-25,19)` | **24** |
| written but absent from the save | **4** - and they are exactly the four port-only cells |

The four lost writes are `(-400,39,308)`, `(-399,39,308)`, `(-396,38,304)`, `(-395,38,307)`; every
one has `wrote=true`, the support below is `minecraft:netherrack` on both sides, and no later `FCELL`
touches those cells. The same shape appears in the Overworld: chunk `(6,-32)` has three
`patch_brown_mushroom` writes and **one** mushroom in the save, and the two lost cells
`(102,68,-509)`, `(97,68,-508)` are the two the port does not have.

So the port's *decision* is not the divergence at these cells. `wrote=true` is
`WorldGenRegion.setBlock`'s return value, which the agent README already warns is not persistence -
but the refusal it documents (`ImposterProtoChunk.allowWrites == false`) is per-chunk, and here 24
writes into the same chunk through the same pass did land.

**Missing observation (blocking the fix).** Vanilla re-runs a block-shape update on the cells
collected by `markPosForPostprocessing` when a proto chunk is promoted
(`ProtoChunk.postProcessGeneration`), *after* `INITIALIZE_LIGHT`; `BushBlock.updateShape` asks
`canSurvive` again against a **lit** engine and turns a mushroom into air. That is the only
mechanism found that makes a landed write disappear, and it partitions the measured cells exactly as
the saved chunks do: the four lost Nether cells are the ones within Manhattan distance 2 of a
`fire`/`soul_fire` block, and the two lost taiga cells are under open sky, while the survivor at
`(99,68,-502)` is under a canopy. The gate for that mark set is
`BlockState.hasPostProcess(BlockGetter, BlockPos)` (`WorldGenRegion.setBlock`'s last statement,
`dtc.q(dcc,jd)Z`, mappings `1213:1213`), which the port does not model at all: its `shape_marks`
carries only `StructurePiece.placeBlock` positions. What is missing is an oracle for that predicate
(alongside `assets/block_predicates_1211.bin`, generated by `tools/block_predicate_oracle.py`) plus
an initialized-light model for the re-check.

Measured cost of landing the brightness half alone (implemented, measured, reverted):

| fixture | before | brightness `=0` only |
|---|---|---|
| Nether `(-25,19)+3x3` ordered | 32 | 32 |
| Village Taiga `(4,-32)+3x3` | 4 | **5** |

It places the canopy mushroom the reference places (+1 correct) and two open-sky mushrooms whose
fate is the unobserved half (+2). A fixture that gets worse is not a fix, so the constant was put
back and the measurement is recorded instead.

## 3. The `(-25,22)` delta layer count: the port's column is shorter, and its own terrain is not

`NCEL` against the port's `CEL_TRACE` on the same pop, two independent captures, identical result:
**226 samples, 149 columns, 7 columns whose per-layer answers differ** - and every difference is the
*top* boundary only, 1 to 4 blocks lower in the port:

| column | reference `NCEL` layer 0 | port `CEL` layer 0 |
|---|---|---|
| `(-386,366)` | 92 | 88 |
| `(-385,363)` | 92 | 88 |
| `(-390,365)` | 92 | 91 |
| `(-385,367)` | 91 | 88 |
| `(-386,361)` | 92 | 90 |
| `(-385,364)` | 92 | 88 |
| `(-385,360)` | 92 | 91 |

`top` agrees (128 on both sides: the Nether roof), so the divergence is cells, not the heightmap.

`CEL_COLUMN_TRACE=-386,366` names the layer that answered:

```
CCELL -386 96 366 0 rc          CCELL -386 87 366 5853 rc       # 5853 = basalt[axis=y]
CCELL -386 92 366 0 rc          CCELL -386 86 366 5853 terrain
CCELL -386 88 366 0 rc          CCELL -386 85 366 5849 terrain  # 5849 = netherrack
```

The port reads air at `88..96`, basalt at 87. Its own `--mode carvers` (un-beardified) dump of the
same chunk has basalt at 91/90 and netherrack at 89/88/87 - which is exactly the reference's answer
(92). So **the port's feature-time terrain is 4 blocks lower than the port's own terrain**, and the
`rc`/`terrain` tags say the whole column came from the terrain read-through cache and `terrain_cell`,
not from a feature write (`FEATURE_CELL_TRACE` over the 7 columns x `y=84..96` logs writes from
`2,4` (large basalt columns) and `0,7` only, all after the delta).

## 4. The first divergence is a beardifier input the reference does not have

`BEARD_TRACE=-25,22` on the port:

```
BEARDPIECE -25,22 minecraft:nether_fossil start=-24,22 bb=[-384, 88, 366, -382, 91, 367]
BEARD -25,22 rigids=1 junctions=0 interesting=73
```

Reading the saved reference chunk's `structures` tag over the whole 9x9 pop footprint:

| | nether_fossil starts |
|---|---|
| reference, chunks `(-28..-20, 16..24)` | `(-22,20)`, `(-22,22)` |
| reference, chunk `(-24,22)` itself | **none** (`structures.starts` empty; `References` empty) |
| port, `BEARD_TRACE=all` | `(-22,20)`, `(-22,22)`, **`(-24,22)`** |

The port places one nether fossil that the reference does not, and its rigid box plus
`ground_level_delta` shapes chunk `(-25,22)`'s columns down by up to four blocks at the chunk's +x
edge - the seven columns above. So the delta's layer walk is *downstream of a structure-placement
difference*, not of `count_on_every_layer`, `MOTION_BLOCKING`, `cel_empty`, or a persistent
heightmap. (The withdrawn page's arithmetic also does not close: `156+2*156+2*73 = 614` and
`82+2*82+2*77 = 400`, not 780/800; the `NCEL` stream is the measurable form of that question and it
says the two sides sample the same 226 columns.)

**Not fixed here.** `NetherFossilStructure#findGenerationPoint`'s validity/height test has to be
read against `src/structure_pipeline.rs`'s fossil arm, and the blast radius is every structure
fixture. It is also not what the residuals in the compared window are about - see §5.

## 5. The 28-cell basalt residual does not reproduce on a complete reference capture

The `(-25,19)+3x3` fixtures carry a 7x7 (=49 chunk) recorded pop order. The reference capture
regenerated for this page decorates **81** chunks (9x9 `(-28..-20, 16..24)`), i.e. the population
footprint vanilla actually completed before the save. Replaying *that* order:

| world / window | command | diffs |
|---|---|---|
| Nether `(-25,19)+3x3`, complete 81-pop capture | `chunk_triage --n 3 --min-y 0 --height 128 --dim nether` | **4** (all `brown/red_mushroom`, no basalt) |
| Nether `(-25,22)` (the delta pop) | `--n 7 --out-of-window report` | 489 |
| Nether `(-24,22)` / `(-25,23)` / `(-24,23)` | same | 483 / 928 / 818 |
| Nether **`(-25,21)`** (where the 28 cells live) | same | **0** |
| Nether 7x7 total | same | 2828 |
| Nether `(-25,19)+3x3` on the **old** 49-pop fixture | `port_triage.py … --pop-order /tmp/rt-4242/nf-pop49.tsv` | 32 |

So the 28-cell `large_basalt_columns` residual is a property of the 49-pop fixture, and the port
matches the reference at those cells when the reference's own footprint is complete. The chunks that
differ on the complete capture are the ones the port manufactures a fossil beard in (`(-25,22)`,
`(-24,22)`, and the z=23 row), all outside the compared window, and they are *not* stable between
two captures of the same seed either: `nat-nf-hooked` vs `nat-nf-str` differ by 56 cells in
`(-25,21)` and agree everywhere else in the window.

## Invalid attempts and corrections (kept, not discarded)

1. **Two captures were generated with `MC_FIXTURE_STAGE=features` while the fixture they were
   compared against is `structures`.** `generate-structures=false` on the reference side against a
   port that always applies beards: the port's `--n 3` diff on that capture was 1821, and the four
   chunks that moved are exactly the ones where beards land. The 7-column delta result survived
   (measured again on the structures-on capture), but the terrain attribution in between was
   invalid until the stage was matched. Lesson: `MC_FIXTURE_STAGE` is part of the comparison
   identity, not a knob.
2. **A reader bug invalidated an intermediate conclusion.** `chunk_triage.load_port_bin(cx, cz, …)`
   loads one chunk; the probe passed the *window origin* and then indexed columns of other chunks,
   which made the port appear to have 28 mushrooms in every chunk of the 3x3 and to disagree with
   its own dump. Re-run per chunk, both statements were false.
3. **The brightness-only change** (§2) was implemented, measured and reverted.
4. `tools/sculk_gate.py` was **not** run: it needs a saved reference region and none was regenerated
   in this lane. `to avoid` re-using `/tmp` tmpfs space, all new captures are under
   `~/mc-spine-local/captures/t6b/` on the root filesystem.

## Regression (unchanged behaviour, so unchanged numbers)

Same binary before and after this lane's source edits; every command is `tools/port_triage.py
--mode full`:

| world | command | before | after |
|---|---|---|---|
| Nether `(-25,19)+3x3`, ordered | `--n 3 --preset nether --pop-order /tmp/rt-4242/nf-pop49.tsv --pop-order-scope recorded` | 32 | **32** |
| Village Taiga `(4,-32)+3x3` | `--n 3` | 4 | **4** |
| stronghold | `-96 35` | 0 | **0** |
| nether fortress | `-25 6 --preset nether` | 0 | **0** |
| ruined portal | `6 7` | 0 | **0** |
| end outer | `10 10 --preset end` | 0 | **0** |

Gates: `tools/layers_check.sh` PASS, `tools/debt_ratchet.sh` 0 over limit,
`python3 tests/gate_scripts.py` PASS, `cargo test --offline --locked --bin mc-spine --
--test-threads=1` green (see the commit message for the count).

## What a next lane needs, in this order

1. A `hasPostProcess` oracle (extend `tools/block_predicate_oracle.py`) and an initialized-light
   model, then land the shape re-check **and** `UNINITIALIZED_RAW_BRIGHTNESS = 0` together: one
   without the other moves a fixture the wrong way, measured above.
2. The `nether_fossil` start at `(-24,22)`: `NetherFossilStructure#findGenerationPoint` against the
   port's fossil arm. This is a structure-parity lane of its own.
3. The 49-pop fixtures should be regenerated at the footprint vanilla actually completes, or the
   comparison should state that a residual measured on them is a footprint artifact.

## 独立再測定（2026-11-16, integration acceptance audit）— 継承・再検証不能

この節は統合時の記録として残すが、**いま独立に再検証することはできない**。当時の capture と
計器出力は `/tmp/captures` 配下にあり、そのディレクトリは現存しない。native fixture
`/tmp/nat-4242-nether-fossil` の本体は残るが、`poporder.tsv` は host 上に見つからない。
したがって下の表は「当時そう測定した」という**継承主張**であり、同条件の再現は保存物からは
閉じない。数字を作り直すこともしない。

| 主張 | 当時の測定 | 記録 |
|---|---|---|
| Nether の `getRawBrightness` は 464/464 = 0 | `nat-nf-str/run.log` の `MLIGHT` = 464行、全て `light=0` | 継承 |
| taiga は 401/403 = 0、1と2が各1 | `nat-taiga-mlight/run.log` の `MLIGHT` = 403行（401×0, 1×1, 1×2） | 継承 |
| 28 writes / 24 in the save / 4 absent = port の余分4セル | FCELL の mushroom(state 2089/2090) = 28、全て `chunk=-25,19 wrote=true`。save 側は 24（red 12 + brown 12）。余分4セル `(-396,38,304) (-400,39,308) (-395,38,307) (-399,39,308)` は4つとも native で air | 継承 |
| 81-pop capture で 4 diffs、basalt なし | `--pop-order nat-nf-str/poporder.tsv --pop-order-scope recorded` → `4 / 294912`、内訳は air→red×2, air→brown×2、basalt 0 | 継承（poporder 不在） |
| NCEL 226 samples / 149 columns（両側） | native `NCEL` in chunk (-25,22) = 226 / 149。port `CEL_TRACE=-25,22` = 226 | 継承 |
| 149列中7列が 1-4 低い、top は 128 で一致 | (x,z,layer) 209 key の突き合わせで差分 7 sample / 7 column、delta -1,-1,-2,-3,-4,-4,-4。両側 `top=128` のみ | 継承 |
| `BEARDPIECE -25,22 minecraft:nether_fossil start=-24,22 bb=[-384,88,366,-382,91,367]` | port の `BEARD_TRACE=all` で同一行 | 継承 |
| `BEARD -25,22 rigids=1 junctions=0 interesting=73` | 同一行 | 継承 |
| reference の fossil start は `(-22,20)` と `(-22,22)` だけ、port は `(-24,22)` も | native の `structures.starts` を 9x9 で走査 → 2件のみ。port の `BEARDPIECE` は (-22,20), (-22,22), (-24,22) | 継承 |

注記（証拠の強さの境界）:

- `BEARD_TRACE` / `CEL_TRACE` は **port 側**の計器で、当時の出力も `/tmp` 側だったため
  capture ディレクトリには残っていない。native 側の `MLIGHT`/`NCEL` 行も同じく現存しない。
- このページの 4 capture は `inputs.sha256` で統合候補の `Agent.java`/`CarveLog.java`/
  `SculkLog.java`/`build_carve_agent.sh` のハッシュと一致し、jar も sha256
  `e3bc5569…`（= pinned 1.21.1 の `59353fb4…` の bundler）に結ばれていた。引用された hash 値が
  候補と一致することは独立レビューが確認しているが、**`inputs.sha256` のファイル自体は現存しない**。
- 28 writes / 24 saved という**事実**は、最終的にどの writer が消すかまでは特定していない。
  `hasPostProcess`/`postProcessGeneration`/lighting は依然として未測定の仮説である（本文 §3 のまま）。
