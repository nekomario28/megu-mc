# The `hasPostProcess` contract: mark at write, consume at promotion, read the light there

The audit this lane implements is
`<local>/audit/convergence-prep-20261116/postprocess_audit.md`
(1.21.1 bytecode, read from the pinned jar). It establishes four facts and this page takes them as
given, re-checking each against the artifact rather than the prose:

| fact | how it is checked here |
|---|---|
| `hasPostProcess` is constant true for exactly `brown_mushroom`, `red_mushroom`, `magma_block`, and nothing else in the jar | the oracle column is exported for all 26,684 states and the three names are re-derived at run time |
| the mark is filed by `WorldGenRegion.setBlock` on the **written state's** predicate, into the chunk that **owns** the cell | `FC::set` files it, and the test asserts the owning chunk |
| the marks are consumed at chunk promotion, after `INITIALIZE_LIGHT`/`LIGHT` | the port's promotion boundary (`shape_marks_ready` = "the whole 3x3 is FULL") is where the pass runs |
| the re-check is `BushBlock.updateShape` → `MushroomBlock.canSurvive` against the **lit** engine | the shape rule asks `raw_brightness`, and the pass answers it from a sample taken at the boundary |

What this lane lands is the contract plus the narrow light read the check needs. It does **not**
land a light engine, and it does not move the `LIGHT` attribution out of HOLD: see §9, "What this does not claim".

## Base state

| | |
|---|---|
| base commit | `39edf3c2979e4533396dbd2d578ba9966a5daf89` (`perf/worldgen-parallel`, clean) |
| native jar | `captures/t6b/nat-nf-str/server.jar` sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| pre-change binary | `red-tree` build of `39edf3c`, sha256 `a5336a2ac8f89255cdaf3b707cbefe0ae2d81b2ef33bf8a32548126870f956d6` |
| post-change binary | sha256 `27dd4666a71b1cd53048d8206c1af27c385ebb03d7eba4e300a53f7a7ea00ff9` (`final/`; an earlier build of the same source, `323aac8e…`, produced the identical port dump sha256 `97eaee35…`, which is what the §6 matrix used) |
| raw logs | `mc-spine-local/audit/haspostprocess-20261118T015855/` (`final/` holds the run against the committed source) |

## 1. The oracle: a column in the asset the port already consults

`tools/block-state-facts-oracle/BlockStateFactsOracle.java` now calls
`BlockStateBase.hasPostProcess(BlockGetter, BlockPos)` (`dtb$a.q`, resolved by parameter types
like the face-sturdy method) for every state against `EmptyBlockGetter`, and
`tools/block_state_facts_from_jar.py` writes it as a sixth byte per state. `BReg::has_post_process`
reads it; no block name is typed into Rust.

The export is additive, and that is verified rather than asserted: regenerating the asset moved
**0 of 26,684** five-byte prefixes (all physical facts are byte-identical), and the new byte is 1
for exactly three states:

```
state id 2089  minecraft:brown_mushroom
state id 2090  minecraft:red_mushroom
state id 12543 minecraft:magma_block
```

The oracle also asserts the set itself (`auditPostProcess`): three blocks, three states, every
other state false, `oak_leaves` the control. A drifted obfuscated name or a fourth setter site
fails the export instead of writing a plausible table.
`generation_contract_tests::has_post_process_is_the_native_three_states_and_no_others` scans the
loaded registry from the Rust side, so the two ends cannot disagree.

## 2. The mark side, and the write timing that changes

`FC::set` is the port's `WorldGenRegion.setBlock`. It now ends with the same two statements the
bytecode has: the **written** state is asked `has_post_process`, and if it answers true the cell is
pushed onto `shape_marks` — the same `ChunkAccess.postProcessing` list the structure pieces already
used, filed on the owning chunk by the existing `Gen::store_shape_mark`. The two mark sources are
disjoint (`SHAPE_CHECK_BLOCKS` is fences/panes/torches/ladders; the `hasPostProcess` set is
mushrooms/magma), so a cell is never marked twice.

Nothing else about a feature write moves: the same features run, in the same order, with the same
draw stream, and the block states they write are unchanged. The difference is *when* a marked cell
is re-derived, and it is exactly the difference vanilla has:

* **before** — the four cells were written by `patch_brown_mushroom` / `patch_red_mushroom` and
  never re-examined, so they stayed in the chunk;
* **after** — they are still written (measured: 28 mushroom writes into chunk `(-25,19)`, 14 brown
  + 14 red), and at the promotion boundary `postProcessGeneration`'s re-derivation removes the four
  whose lit brightness is `>= 13`.

Cells that can now differ from the old populate-time behaviour, stated exhaustively:

1. a cell written with one of the three states, whose `updateShape` changes it at promotion
   (mushrooms → air; `magma_block` never changes, see §4);
2. a cell that a *shape* rule reaches at promotion, if the light read changes that rule's answer —
   no other rule in the pass reads light, so this is empty today.

The write-timing evidence is two ordered trace line sets from the same run
(`FEATURE_WRITE_TRACE=-25,19`):

```
FWRITE -400 39 308 state=2089 feature=9,7 pop=-25,19    # patch_brown_mushroom writes it
FWRITE -399 39 308 state=2089 feature=9,7 pop=-25,19
FWRITE -396 38 304 state=2090 feature=10,7 pop=-25,19   # patch_red_mushroom writes it
FWRITE -395 38 307 state=2090 feature=10,7 pop=-25,19
...
FWRITE -400 39 308 state=0 feature=2,9 pop=-24,20       # the promotion pass removes it
FWRITE -399 39 308 state=0 feature=2,9 pop=-24,20
FWRITE -395 38 307 state=0 feature=2,9 pop=-24,20
FWRITE -396 38 304 state=0 feature=2,9 pop=-24,20
```

`pop=-24,20` is the pass that completed the 3x3 around `(-25,19)`, which is the chunk vanilla
promotes there. (`feature=2,9` is a stale index: the feature counter is not advanced by a
non-feature write, so `pop=` is the field that identifies the writer.)

## 3. The consumption side: one pass, at the boundary, with a light sample

`promote_full_chunks` is the boundary, extracted from the tail of `populate` so it has a name and a
test (it is still the **only** pass over the marks; no second owner was created):

```
flush ctx.shape_marks -> Gen::store_shape_mark          # markPosForPostprocessing(getChunk(pos))
ctx.g.pop_active = None                                 # a LevelChunk write, not a region write
for each of the 3x3 whose marks are ready (= FULL):
    take marks; sort by section (stable -> mark order inside a section)
    begin_promotion_pass(cx, cz, &marks)                # sample the light, once
    postprocess_shape_marks(...)                        # updateFromNeighbourShapes + set(20)
end_promotion_pass()
```

`pop_active = None` before the first rewrite is what keeps the pass from marking its own writes: a
mark filed there would need a second pass over an already-promoted chunk, which is the failure the
"no marks leaking / no double-processing" tests pin from both sides.

The light read is `FC::raw_brightness`, and it exists only inside a promotion pass:

* **block light** — a multi-source flood from every state in the promoted chunk's 3x3 with
  `getLightEmission() > 0`, attenuating `max(1, getLightBlock())` per entered cell
  (`LayerLightEngine`'s rule). Sources are enumerated from the write layers (`wr` for the chunk
  being decorated, `feat_c` buckets for the neighbours); the flood's cell reads go through
  `FC::get`, so terrain occludes and terrain lava is reached once the flood arrives.
* **sky light** — `15 - sum(getLightBlock)` of the column above, capped at 15, only in a dimension
  with a sky layer.
* sampled **once per promoted chunk, before anything is rewritten**, because
  `postProcessGeneration` sets with flags `20` (`UPDATE_INVISIBLE | UPDATE_KNOWN_SHAPE`), which
  does not relight. A later mark therefore reads the same field the first one did, and the pass
  cannot depend on mark order.
* `None` outside a pass. The mushroom rule then leaves the state alone rather than guessing; that
  is what keeps template placement (`apply_shape_passes`, which runs inside `populate`) inert, and
  it is the same distinction `generation_providers::uninitialized_raw_brightness` records on the
  write side.

The rule itself is `MushroomBlock`'s (`dla.a(dtc,dcz,jd)`) with `BushBlock.updateShape`'s
`!canSurvive -> air`:

```java
support.is(MUSHROOM_GROW_BLOCK)
   || (level.getRawBrightness(pos, 0) < 13 && isSolidRender(support, below))
```

`POSTPROC_TRACE=<cx>,<cz>` prints the cell, the transition and the brightness the pass read — the
one number that has no other observable. On the Nether window it names **four** cells in the whole
chunk, each at `light=13`:

```
POSTPROC -400 39 308 from=minecraft:brown_mushroom to=minecraft:air light=Some(13)
POSTPROC -399 39 308 from=minecraft:brown_mushroom to=minecraft:air light=Some(13)
POSTPROC -396 38 304 from=minecraft:red_mushroom   to=minecraft:air light=Some(13)
POSTPROC -395 38 307 from=minecraft:red_mushroom   to=minecraft:air light=Some(13)
```

That is the audit's "a 4-cell post-hoc fit" converted into a read: `15 - 2` from the `fire` two
cells away, at the boundary, measured per cell.

## 4. `magma_block`: marked, consumed, never moved

`MagmaBlock.updateShape` (`dkr.a(dtc,ji,dtc,dcx,jd,jd)`) is

```java
if (direction == UP && neighborState.is(Blocks.WATER)) level.scheduleTick(pos, Blocks.WATER, 20);
return super.updateShape(...);
```

a scheduled tick, not a state change, so the pass rewrites nothing for a marked magma cell.
`promotion_consumes_a_magma_mark_without_moving_it` pins both halves (the mark is filed, the state
survives) and `postprocess_leaves_a_marked_magma_block_unchanged` pins the rule on a scripted scene.
This is the arm that would silently remove basalt-delta magma if the light read were applied to
every marked state, which is why the `basalt_deltas` fixture is in the regression matrix (§6).

## 5. Re-measurement of the residual

The residual, restated on artifacts rather than on an inherited log: chunk `(-25,19)` of the
`nat-nf-str` capture (seed 4242, frozen, `structures`, the capture's own 81-chunk `poporder.tsv`).

| | mushroom cells in the port dump | native saved chunk |
|---|---|---|
| before (`39edf3c`) | **28** | 24 |
| after | **24** | 24 |

and the 24 are the same 24 cells — the grid diff says so directly:

| | diff |
|---|---|
| `chunk_triage --n 3 --min-y 0 --height 128 --dim nether` before | **4 / 294,912** |
| after | **0 / 294,912** |

The four closed cells are `(-400,39,308)`, `(-399,39,308)`, `(-396,38,304)`, `(-395,38,307)` — the
same four the earlier page recorded, and the four §3's trace names. `28 writes / 24 saved` is now
the port's own shape as well: 28 writes measured (`FEATURE_WRITE_TRACE`), 24 in the dump.

The fixture-level check that the *light read* is the cause, not the mark plumbing alone: with the
registered mutation `lit < 13` → `lit <= 13`, the same command on the same capture returns
**4 / 294,912** again, with the same four cells (`mutant-triage.log`). The source was restored
byte-for-byte (`src/template_structures.rs` sha256 `fea8f8e1…` before and after) and the clean
binary rebuilt to `323aac8e…`.

## 6. Regression matrix, measured twice

Every row was run on **both** binaries with the same command: `before` is the `39edf3c` build
(the `red-tree` worktree, whose only local edit is a `#[cfg(test)]` function the bin build does not
compile), `after` is this tree. Nothing is inherited from an earlier page.

| fixture | command | before | after |
|---|---|---|---|
| Nether `(-25,19)+3x3` (81-pop capture) | `--n 3 --preset nether --pop-order <cap>/poporder.tsv --pop-order-scope recorded` | **4** | **0** |
| Village Taiga `(4,-32)+3x3` | `--n 3` | 25 | 25 |
| `basalt_deltas` `(-34,1)` 0..255 | `--n 1 --preset nether --height-window dimension` | 0 | 0 |
| nether fossil `(-22,20)` 0..255 | same | 0 | 0 |
| nether fortress `(-25,6)` 0..255 | same | 0 | 0 |
| ruined portal `(6,7)` | `--n 1` | 0 | 0 |
| stronghold `(-96,35)` | `--n 1` | 0 | 0 |
| end outer `(10,10)` | `--n 1 --preset end` | 0 | 0 |
| end city `(65,45)` 0..255 | `--n 1 --preset end --height-window dimension` | 0 | 0 |

The `basalt_deltas` row is the one that would catch a light-driven `canSurvive` applied to magma;
the stronghold row is the fence/multiface shape pass, which must not move; the taiga row is the
Overworld sky model, which changes nothing because the port only ever *places* a mushroom on a
`#minecraft:mushroom_grow_block` there (its write-time brightness is the uninitialized value).

## 7. Tests

| test | what it pins |
|---|---|
| `has_post_process_is_the_native_three_states_and_no_others` | the oracle column against the native set, from the loaded registry |
| `a_written_post_process_state_files_a_mark_on_the_owning_chunk` | mark at write, owning chunk, and the two negatives (a non-set state; a write outside a FEATURES window). **Red on unmodified `39edf3c`** |
| `promotion_removes_a_mushroom_the_lit_engine_reaches` | the whole contract end to end: same write, light 13 vs 12, air vs kept |
| `promotion_consumes_a_magma_mark_without_moving_it` | the third block: consumed, unchanged |
| `the_promotion_light_sample_is_taken_once_and_only_inside_the_pass` | the sample is the state at promotion, `None` outside the pass, 0 outside the flood's reach |
| `the_promotion_boundary_files_no_mark_and_consumes_its_marks_once` | no mark out of the pass, no second rewrite, the window closed |
| `postprocess_removes_a_mushroom_only_where_the_lit_engine_reaches_it` | the shape rule against `isSolidRender`, `#mushroom_grow_block` and `None` |
| `postprocess_leaves_a_marked_magma_block_unchanged` | `MagmaBlock.updateShape` moves nothing |

**Pre-change red log** (`red-test.log`, in the `red-tree` at `39edf3c`): the mark test, unmodified
tree, `cargo test -- --test-threads=1 a_written_post_process_state_files_a_mark_on_the_owning_chunk`

```
test feature::generation_contract_tests::a_written_post_process_state_files_a_mark_on_the_owning_chunk ... FAILED
panicked at src/generation_contract_tests.rs:7351:5:
assertion `left == right` failed: a hasPostProcess state must be marked where it was written
  left: []
 right: [(3, 200, 3)]
test result: FAILED. 0 passed; 1 failed; 646 filtered out
```

The consumption-side tests cannot compile against `39edf3c` at all (the boundary function, the
light sample and the `raw_brightness` seam do not exist there), so the literal red is the mark half
plus the fixture-level measurement of §5 — 4 cells on the unmodified binary, 0 after.

## 8. Mutation entries

Two entries, both anchored in this contract, appended as one block at the end of
`tools/mutation_check.sh` (registry 452 → 454):

```
check "only a hasPostProcess state is marked for post-processing" \
  src/generation_context.rs '        if self.g.pop_active.is_some() && reg.is_some_and(|r| r.has_post_process(st)) {' \
  '        if self.g.pop_active.is_some() {' \
  generation_contract_tests::a_written_post_process_state_files_a_mark_on_the_owning_chunk

check "the promotion shape pass keeps a mushroom whose raw brightness is under 13" \
  src/template_structures.rs '        return if lit < 13 && reg.is_solid_render(below) {' \
  '        return if lit <= 13 && reg.is_solid_render(below) {' \
  generation_contract_tests::promotion_removes_a_mushroom_the_lit_engine_reaches
```

`MUTATION_STABILITY=1`, red/red/green per entry — measured, one filtered campaign per entry
because the two named tests share no token:

```
ok    only a hasPostProcess state is marked for post-processing
mutation check: 1 proved, 0 not a gate, 0 parked, 0 skipped
ok    the promotion shape pass keeps a mushroom whose raw brightness is under 13
mutation check: 1 proved, 0 not a gate, 0 parked, 0 skipped
```

`tests/gate_scripts.py` (the whole campaign, its own filters) is `450 proved, 0 not a gate, 0
parked, 4 skipped`; the four skips are entries whose anchors are already absent at `39edf3c`
(checked against `git show HEAD:src/main.rs`), i.e. pre-existing, not caused by this lane.

## 9. What this does not claim

* **The `LIGHT` attribution stays HOLD.** What is measured here is the port's own promotion-boundary
  read (13 at four cells), not the reference's `getRawBrightness` at `postProcessGeneration`. The
  reference's value at that moment is still unobserved; the contract is implemented because it is
  the vanilla mechanism, and the four cells closing is a consequence of the model agreeing with the
  saved chunk, not a measurement of the reference's engine.
* **The sky term is a model, not a light engine.** `15 - sum(getLightBlock)` down the column is the
  straight-down value; it does not spread sideways from a lit neighbour column. No shape rule in
  the pass needs that today, and the Overworld rows are unchanged.
* **Emitters are enumerated from the write layers.** A terrain-only emitter that no feature or
  carver wrote is not a source (the flood still reads terrain, so it occludes and it can be *lit*
  by a write-layer source). On the measured window the four cells' source is a `fire` written two
  steps away in the same pass, which is in the layer.
* **The village-taiga placement residual does not move.** `(99,68,-502)` is a mushroom the
  reference *places* and the port refuses at write time; the promotion pass can only remove. That
  is a write-time brightness question (`uninitialized_raw_brightness`'s 15 for a sky dimension,
  which the 2026-11-16 hook measured as 0), and it is out of scope here.
* **The `nether_fossil` start at `(-24,22)` is untouched** — separate mechanism, separate lane.
* The pass now costs a light sample per promoted chunk that carries a mushroom or magma mark: the
  Nether window's port dump went from 19.9 s to 32.1 s on this host (same window, same binary
  flags, one seed). Not a budget claim, just the number.

## 10. Gates

| gate | result |
|---|---|
| `cargo test --offline --locked --bin mc-spine -- --test-threads=1` | `653 passed; 0 failed; 1 ignored` (654 tests; `39edf3c` has 646, this lane adds 8) |
| same with `--test-threads=8` | `653 passed; 0 failed; 1 ignored` |
| `tools/layers_check.sh` | PASS (44 modules, 111 edges; no module added) |
| `tools/debt_ratchet.sh` | 0 over limit |
| `python3 tests/gate_scripts.py` | PASS (see §8 for the skip count) |
| the two mutation entries | 2 proved, `MUTATION_STABILITY=1` |

The one ignored test is the pre-existing parked gate `debt_ratchet.sh` reports.

`haspostprocess-contract-20261118.run/receipt.json` is `tools/evidence_receipt.py`'s
source-bound receipt for the eight contract tests: `status: PASS`, `head 6b284f9`, `dirty 0`,
`tests_total 654`, eight filters run and green, test binary `840cd82b…`.

## Reproduce

```sh
# oracle (needs the pinned bundler jar; nothing is written into the tree without --write)
MC_SPINE_VANILLA_SERVER_JAR=captures/t6b/nat-nf-str/server.jar \
  python3 tools/block_state_facts_from_jar.py        # compares; --write regenerates the asset

# the residual, before and after
CAP=<local>/captures/t6b/nat-nf-str
tools/port_triage.py "$CAP/world" -25 19 --n 3 --preset nether \
  --pop-order "$CAP/poporder.tsv" --pop-order-scope recorded --binary <binary>

# the pass, per cell, with the brightness it read
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_PRESET=nether \
MC_SPINE_WG_DUMP=/tmp/dump.bin MC_SPINE_WG_DUMP_WINDOW=noise \
MC_SPINE_WG_DUMP_X=-25 MC_SPINE_WG_DUMP_Z=19 MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_POP_ORDER="$CAP/poporder.tsv" MC_SPINE_POP_ORDER_SCOPE=recorded \
POSTPROC_TRACE=-25,19 ./target/debug/mc-spine

# the two entries (the named tests share no token, so one filtered campaign each)
MUTATION_FILTER=a_written_post_process_state_files_a_mark_on_the_owning_chunk \
  MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline
MUTATION_FILTER=promotion_removes_a_mushroom_the_lit_engine_reaches \
  MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline
```

One seed, one dimension, one host.
