# The spurious `nether_fossil` was a biome gate asked at the wrong place

§4 of [`light-and-lost-writes-20261116.md`](light-and-lost-writes-20261116.md) left one bounded
question open: seed 4242's chunk `(-24,22)` in the Nether holds no `nether_fossil` start in the
reference, while the port placed one (`bb=[-384, 88, 366, -382, 91, 367]`) and its `beard_thin`
shaped neighbouring columns. The cause is not the noise column, the height provider, the salt or
the scan - all three of those agree - it is **where the structure's biome gate is asked**.

## 1. What vanilla asks

`Structure.findValidGenerationPoint` (`ejr.b(ejr$a)`, official mappings `/var/tmp/mcm/server.txt`)
is `findGenerationPoint(ctx).filter(...)`, and the filter (`ejr.a(ejr$b, ejr$a)`) reads:

```
0: invokestatic  #249 ejr$b.b:()Ljd;              // stub.position()
...
17: invokevirtual #260 jd.u:()I                    // pos.getX()
20: invokestatic  #265 jx.a:(I)I                    // QuartPos.fromBlock  (jx = QuartPos)
   ... same for getY()/getZ() ...
41: invokevirtual #278 dzm.b:()Ldef$f;             // randomState.sampler()
44: invokevirtual #284 dea.getNoiseBiome:(IIILdef$f;)Ljm;   // BiomeSource, unblended
47: invokeinterface #290 java/util/function/Predicate.test
```

So the gate samples the **raw noise biome at the quart of the stub position** that the structure's
own `findGenerationPoint` returned. Two consequences the port did not model:

* the sampled position is the start, not the chunk centre, and its `y` is the start's `y`;
* the source is `BiomeSource.getNoiseBiome`, not `BiomeManager.getBiome` (no blending).

`NetherFossilStructure` (`ema`) returns `new BlockPos(i, l, j)` where `i`/`j` are
`chunkPos.getMinBlockX/Z() + random.nextInt(16)` and `l` is the noise-column scan result
(`ema.a(ejr$a)`: `ehx.a(Layw;Ldzv;)I` = `HeightProvider.sample`, `duz.a(IILdcy;Ldzm;)Lddi;` =
`getBaseColumn`, `dtc.i()` = `isAir`, `dga.dW` = `Blocks.SOUL_SAND`, `dtc.d(dcc,jd,ji)` =
`isFaceSturdy` against `dcl.a` = `EmptyBlockGetter.INSTANCE`, loop floor `duz.f()` = `getSeaLevel`).
Its biome tag `#minecraft:has_structure/nether_fossil` holds **only** `soul_sand_valley`.

## 2. What the port asked

`src/structure_pipeline.rs::try_generate`, `Kick::Template`, ran its biome test **before**
`structure_template::find_generation_point`, at a fixed point in the chunk:

```rust
if let Some(biome) = fc.biome(cx * 16 + 8, 64, cz * 16 + 8) {   // centre, y = 64
    if !def.biomes.is_empty() && !def.biomes.contains(&biome) { return None; }
}
```

For every template structure kind that is a different question, and in the Nether it is a different
*answer*. Measured on seed 4242, chunk `(-24,22)` (scan `x=-382 z=367`, `y0=96`, scan stops at
`y=88` on `air` above `netherrack`):

| position | port quart read | native quart read | in the tag? |
|---|---|---|---|
| stub `(-382, 88, 367)` | `basalt_deltas` | `basalt_deltas` (quart `-96,22,91`) | **no** |
| centre `(-376, 64, 360)` | `soul_sand_valley` | `soul_sand_valley` (quart `-94,16,90`) | yes |

The native values come from a one-off probe built the way
`tools/structure-oracle/NetherBiomeOracle.java` builds its source and `RandomState` (real
`MultiNoiseBiomeSource`, real nether `NoiseGeneratorSettings`, seed 4242), calling
`BiomeSource.getNoiseBiome(qx, qy, qz, randomState.sampler())` - the same call the gate's predicate
makes. `chunk_bytes`/`parse_nbt` over `world/DIM-1` then says the same thing from the save side:

| native chunk | `structures.starts` |
|---|---|
| `(-22, 20)` | `nether_fossil`, child `[-356, 55, 319, -350, 61, 324]` |
| `(-22, 22)` | `nether_fossil`, child `[-337, 76, 355, -333, 80, 357]` |
| `(-24, 22)` | empty (`References` empty too) |

The port reproduces both accepted boxes exactly (its scan finds `(-350,55,324)` and
`(-337,76,357)`, both `soul_sand_valley`), so the rng draw, the height sample, the base column and
the scan predicate agree. Only the gate's position differed.

## 3. The fix

The centre pre-check is gone; the gate runs after `find_generation_point`, on the stub, in
`try_generate` (`src/structure_pipeline.rs`). Same set membership test as before, same fail-closed
read when the biome is unknown. No other `try_generate` arm changes: `Kick::Mineshaft`,
`Kick::SinglePiece`, `Kick::Jigsaw` and `Kick::Wave3a` each gate at their own vanilla position
already, and `Kick::Template` is the only arm that used the centre.

Regression test: `feature::pipeline_tests::nether_fossil_biome_gate_reads_the_start_not_the_chunk_centre`
- no `nether_fossil` start at `(-24,22)`; both native boxes at `(-22,20)`/`(-22,22)`; the gate's own
read of `(-382,88,367)` is `basalt_deltas` while `(-376,64,360)` is `soul_sand_valley`; and
`beard_chunk_data(-25,22)` has no rigid left.
Mutation (`tools/mutation_check.sh`, entry "a template structure's biome gate reads the start, not
the chunk centre", `MUTATION_STABILITY=1`): 1 proved, 0 not a gate. The mutant restores
`.biome(cx * 16 + 8, 64, cz * 16 + 8)` and the test goes red twice.

## 4. Measurements

Both binaries are builds of this tree; `before` = `c40317b037e28007…`, `after` =
`c2eab18a27c358bd…`. Capture: `mc-spine-local/captures/t6b/nat-nf-str` (seed 4242, Nether,
`MC_FIXTURE_STAGE=structures`, `poporder.tsv` = the 81 recorded pops), compared with
`tools/port_triage.py … --preset nether --pop-order …/poporder.tsv --pop-order-scope recorded`.

| window | before | after |
|---|---|---|
| `(-25,19)+3x3` | 4 | **4** (the four mushroom cells of §2, untouched) |
| `(-25,22)` (the 7-column delta chunk) | **489** | **0** |
| `(-24,22)` (held the spurious start) | **483** | **0** |
| 7x7 full native footprint `(-27,17)+7x7` | **2828** | **4** |

The 7x7 scoreboard, which is where the 2828 of §5 in the parent page came from:

| chunk | before | after |
|---|---|---|
| `(-25,19)` | 4 | 4 |
| `(-25,22)` | 489 | 0 |
| `(-24,22)` | 483 | 0 |
| `(-23,22)` | 1 | 0 |
| `(-26,23)` | 54 | 0 |
| `(-25,23)` | 928 | 0 |
| `(-24,23)` | 818 | 0 |
| `(-23,23)` | 51 | 0 |

Every cell the fix removes is a cell the reference does not have; nothing in the 7x7 window got
worse. The unchanged 4 are the `brown/red_mushroom` writes the parent page §2 explains (a
`hasPostProcess`/light re-check the port does not model), not a placement question.

Blast radius, measured as a byte comparison of the same dump between the two binaries
(`MC_SPINE_WG_PRESET=<d> MC_SPINE_WG_DUMP`), not as a diff count:

| dimension | window (8x8 chunks) | result |
|---|---|---|
| Overworld, full (`features`+beards) | `(0,0)`, `(-64,-64)`, `(64,64)`, `(128,0)`, `(0,128)` | **identical** |
| Nether, full | `(-28,16)` (the fossil case) | differs in exactly 7 chunks: the fossil beard + the `large_basalt_columns` cells that read the beard-shaped surface |
| Nether, full | `(-42,-30)`, `(20,20)`, `(-80,40)`, `(60,-60)` | **identical** |

The `(-42,-30)` Nether window is the one with a `ruined_portal_nether` (the other `Kick::Template`
kind that exists in the Nether), so a nether temple-style gate at the surface still answers the same.

Village taiga, the only Overworld fixture with a port-vs-native comparison on disk:
`tools/port_triage.py … 4 -32 --n 3 --pop-order …/order.tsv --pop-order-scope recorded` is **4 → 4**
and the dump is byte-identical (`0d39afbc59ab9abd…`), i.e. the village/vegetation residuals of
`village-taiga-support-rules-and-placement-order-20261116.md` are untouched.

Gates: `tools/layers_check.sh` PASS, `tools/debt_ratchet.sh` 0 over limit,
`cargo test --offline --locked -j 6 -- --test-threads=1` 558 passed / 1 ignored,
`--test-threads=8` 558 passed / 1 ignored.

## 5. What is still not measured

1. **Blended vs raw biome.** The gate now asks the right *position*, but the port still reads
   `BiomeManager.getBiome` (`FC::biome`, which prefers the blended leaf) where vanilla's predicate
   reads `BiomeSource.getNoiseBiome`. At the four positions probed here the two agree, and no
   fixture moved, so this lane did not also change the sampler - a boundary quart could still
   answer differently and that is a separate, unmeasured question.
2. **The scan predicate.** The port tests `!Block.hasDynamicShape() && isCollisionShapeFullBlock`
   (`assets/template_solid_1211.bin`) where vanilla tests `isFaceSturdy(EmptyBlockGetter, pos, UP)`.
   They differ for shapes that are sturdy but not full (a bottom slab). `getBaseColumn` only ever
   yields the settings' default block, an aquifer substance, or air, so no cell of a scanned column
   can carry such a shape today, and all three probed chunks agree block-for-block; the bound is
   the base column's state set, not a proof about every state.
3. **One seed.** Every number above is seed 4242. The divergence is a property of "a Nether fossil
   whose scan lands on a `basalt_deltas`/`nether_wastes`/`crimson_forest`/`warped_forest` quart while
   its chunk centre is `soul_sand_valley`", which is common, but this lane did not enumerate others.
4. **The 7x7 grid needed `--out-of-window report`**: the capture's `initialize_light`-status ring and
   a few native cells above `y=127` fall outside the port's `0..127` noise window, and those cells
   are excluded from every count above (1 cell in `(-27,19)`).
