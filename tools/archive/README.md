# tools/archive — retired one-off dev probes

**These files are archived offline development probes. None of them is an active
CI gate.** Nothing under `.github/`, `tests/`, `tests/verify.sh`, `tools/*.sh`
or any other gate invokes, imports or checks a script in this directory. A
green CI run says nothing about them, and breaking one cannot fail the build.

/ ここにあるのは**引退したオフライン開発用プローブ**です。CI ゲートではなく、
ワークフロー・`tests/verify.sh`・他の `tools/` スクリプトからは参照されません。

## Why they are here

Each file below was a one-off measurement probe: a script written to answer one
question against the pinned vanilla 1.21.1 jar (or a live server), used once
while landing a subsystem, and then left in `tools/` next to the real gates.
They accumulated real cost — a reader could not tell a load-bearing oracle from
a spent probe, and `tools/` listing showed ~35 dead entry points mixed in with
the gates that must stay green.

They were moved here only after a repo-wide reference audit found **zero**
references, for every one of them, in both spellings that could bind them:

* the filename (`foo_oracle.py` — as invoked by `python3 tools/...`, a workflow
  step, a shell script, a JSON/YAML manifest, or a doc), and
* the bare module name (`import foo_oracle` / `from foo_oracle import ...` —
  many tests and tools put `tools/` on `sys.path` and import siblings).

The audit covered `.github/`, `tests/`, `tools/`, `src/`, `docs/`, `specs/`,
`README*`, `audit/` and every tracked file (`rg` plus an independent `git grep`
over tracked content), and additionally confirmed:

* no candidate has a `tests/<name>_test.py` partner,
* no candidate is reached by a dynamically built path (`Path(...) / f"{name}.py"`),
* no candidate is the sole reader of a committed fixture — every
  `tests/data/*.tsv` they reference is absent, i.e. these probes never produced
  reference data that is still tracked,
* no ratchet or layer gate counts them: `tools/debt_ratchet.sh` scans only
  `*.rs`, `tools/check_layers.py` inspects Cargo targets, and
  `tests/verify_gate_coverage_test.py` only enumerates `tests/*.py`.

## Important: their old paths still appear in `evidence/`

`evidence/` was excluded from the reference audit by design — those pages are
immutable historical records of a measurement that already happened, and a
receipt hash or a past run command is a statement about the past, not a live
call site. Several archived probes are named there **under their old path**:

| archived probe | evidence records that mention it |
| --- | --- |
| `falling_oracle.py` | `evidence/gameplay/falling-blocks/README.md`, `receipt.json` |
| `leaf_tick_oracle.py` | `evidence/gameplay/forest-r1/feature-contracts.md` |
| `light_parity_probe.py` | `evidence/gameplay/light-update/README.md`, `evidence/gameplay/light/vanilla-comparison.md` |
| `light_update_probe.py` | `evidence/gameplay/light-update/receipt.json` |
| `ore_air_oracle.py` | `evidence/gameplay/forest-r1/ore-air.md`, `ore-air/source-hashes.sha256` |
| `port_runtime_stage_check.py` | `evidence/gameplay/forest-r1/feature-contracts.md` |
| `single_piece_oracle.py`, `template_structure_oracle.py` | `evidence/review/repository-convergence-20261116/inventory.json` |
| `vanilla_chunk_blocks.py` | `evidence/gameplay/light/vanilla-comparison.md` |
| `vanilla_ore_isolate.py` | `evidence/gameplay/feature-parity/README.md` |
| `wire_census.py` | `evidence/foundation/wire-census-20261123.md` |

If you are following an old reproduction command from one of those pages, the
script now lives at `tools/archive/<name>.py`. The historical text was left
untouched rather than rewritten: a receipt's recorded path and hash describe the
tree at capture time, so editing it would falsify the record.

## Index (32 probes, 2,133 LOC)

Jar-backed `*_oracle.py` probes build on `tools/vanilla_java.py`, `tools/feature_oracles.py`
and the harnesses in `tools/structure-oracle/`. That supporting code is **not**
archived — it is shared by the live oracles that remain in `tools/`.

| probe | LOC | what it measured |
| --- | --- | --- |
| `template_structure_oracle.py` | 405 | `BlockAgeProcessor` / extended `StructureTemplate` placement, native |
| `falling_oracle.py` | 169 | Same-input falling-block behaviour (floor + sand, server block updates) |
| `vanilla_chunk_blocks.py` | 120 | Decode a live server's chunk sections into block ids, column by column |
| `wire_census.py` | 118 | Histogram every S2C packet id actually emitted, per phase |
| `leaf_trace_replay.py` | 99 | Replay actual writes from a leaf trace against native calculations |
| `port_runtime_stage_check.py` | 95 | Frozen / uninterrupted / restarted port dumps vs native saves |
| `fluid_trace_vectors.py` | 93 | Bounded, ordered native fluid-tick scenes from a 7x7 trace |
| `template_entity_oracle.py` | 90 | Native entity-offset transform of a structure template |
| `light_update_probe.py` | 84 | Boundary generated-light update (`0x2a`) with a flat-world control |
| `resumed_stage_check.py` | 75 | Frozen capture and both checkpoints from its resumed copy |
| `light_parity_probe.py` | 69 | Canonical dump of the light a server sends for one chunk |
| `vanilla_ore_isolate.py` | 68 | Data pack leaving exactly one `ore_*` feature running |
| `leaf_tick_compare.py` | 47 | Generated leaf-tick identities and per-chunk order vs a native trace |
| `basalt_pillar_oracle.py` | 41 | `basalt_pillar` cells and RNG consumption |
| `fortress_monument_oracle.py` | 38 | Fortress/monument piece-tree and placement vectors |
| `single_piece_oracle.py` | 37 | Single-piece procedural box structure vectors |
| `leaf_distance_oracle.py` | 35 | `TreeFeature.updateLeaves` with vanilla block tags bound |
| `leaf_tick_oracle.py` | 33 | Scheduled-leaf distance calculation |
| `nether_biome_oracle.py` | 32 | Nether biome selection with its seeded `RandomState` |
| `leaf_edge_oracle.py` | 31 | Tree leaf shape-edge scheduled block ticks |
| `monster_room_oracle.py` | 31 | Dungeon cells and RNG from the pinned `MonsterRoomFeature` |
| `multiface_oracle.py` | 30 | Multiface growth/spreading over controlled worlds |
| `nbt_codec_oracle.py` | 30 | `NbtIo` typed values, malformed input, Java modified UTF-8 |
| `normal_noise_oracle.py` | 30 | `NormalNoise.create` over a `LegacyRandomSource` |
| `ore_air_oracle.py` | 30 | Ore exposure decisions and following RNG |
| `blended_noise_oracle.py` | 29 | Dimension `BlendedNoise` double results |
| `chorus_plant_oracle.py` | 29 | Chorus feature states and recursive RNG consumption |
| `concentric_rings_oracle.py` | 29 | Native ring positions and RNG-sensitive fixed-biome relocation |
| `end_biome_oracle.py` | 29 | The five End biomes through native source and `RandomState` |
| `end_island_feature_oracle.py` | 29 | End island feature writes and RNG state |
| `end_islands_oracle.py` | 29 | Seeded End island density and float height results |
| `glowstone_oracle.py` | 29 | `glowstone` feature cell sets and RNG consumption |

## Deliberately left in `tools/` (not archived)

| file | why it stayed |
| --- | --- |
| `tools/jigsaw_oracle.py` | Referenced by `tools/structure-oracle/JigsawOracle.java` — its runtime error message tells the operator to run this script instead of the class. Moving it would leave a live message pointing at a missing path. |
| `tools/stronghold_oracle.py` | Referenced by `tools/structure-oracle/StrongholdOracle.java` ("see `tools/stronghold_oracle.py` for the schema"); the pairing is the documented contract for that harness. |
| `tools/jvm_eval/bench_jvm_baseline.py` | Sole consumer of the tracked harness `tools/jvm_eval/NeoForgeEvalHarness.java`, so moving it would orphan that Java file. It also anchors paths at `ROOT = parents[2]`, which is only repo root while it lives in `tools/jvm_eval/`; archiving it would silently repoint `target/tmp` and its java sources at the wrong directory. |

## They are not runnable in place

The files were moved byte-for-byte, without editing. Most of them anchor paths
at the depth they had while in `tools/` — `ROOT = Path(__file__).resolve().parents[1]`
for the repo root, and `sys.path.insert(0, str(Path(__file__).resolve().parent))`
to import siblings such as `vanilla_java` and `feature_oracles`. From
`tools/archive/` those anchors resolve one level too deep (to `tools/`, not the
repo root), and the sibling imports do not resolve at all.

So do not run `python3 tools/archive/<name>.py` and expect the measurement it
documents. Either restore it first (below), or expect to fix its path anchor.

## Reviving a probe

Restoring one is a plain move back to the depth it was written for:

```bash
git mv tools/archive/<name>.py tools/<name>.py
```

Do that only together with a live caller (a test, a `verify.sh` stage, or a
workflow step). Re-adding a probe to `tools/` without a caller is what this
archive exists to prevent: `tools/` should mean "things the gates run", not
"things someone once ran".

Most of the jar-backed probes need the pinned 1.21.1 server jar and a JDK, and
several also need a built `megu` binary or a reachable server port; see the
module docstring at the top of each file for its exact prerequisites and
output format.
