# M39 — P1-4 real-chunk Fossil acceptance gate (2026-11-23)

Status of checklist item **P1-4 実チャンクFossil受け入れ**
(`specs/implementation-checklist.md`, spec `specs/template-finalization/slices/04-chunks.md`).

## What the checklist item requires

> 実際にFossilが置かれるnativeの完全chunkを採取し、同一stage・source順・marginで
> 全state、heightmap、block entity、pending tickを比較。続けてruntime tickと
> save/reload、light・wireは**別のgate**で評価する。scripted-level oracleの緑だけで閉じない。

Before this change the only fossil evidence was (a) the scripted-world oracle
(`tools/fossil_oracle.py`, `fossil_cases_1211.json` — no real chunk is ever
generated) and (b) a one-shot local triage recorded in
`evidence/gameplay/feature-parity/template-fossil-p1-20261120.md` (65,536 cells, 0
diffs, measured 2026-11-20 at port `5759cbef`, not repeatable without the local
capture). Neither is a gate.

## What exists now

A committed acceptance gate that replays the real chunk through the port's
production offline-generation path and compares it against committed native
fixtures:

- `tools/fossil_chunk_gate.py` — the gate. Drives `target/debug/megu` with
  `MEGU_SEED=4242 MEGU_WG_PROFILE=0 MEGU_WG_PRESET=nether MEGU_WG_DUMP{,_CANON,_HM}
  MEGU_WG_FLUID_TICKS MEGU_WG_DUMP_WINDOW=world` for chunk `(-22,20)` and
  `MEGU_POP_ORDER` + `MEGU_POP_ORDER_SCOPE=exact` replaying the recorded 9-chunk
  decorate order, then compares three arms and emits a JSON report
  (exit `0` MATCH / `1` DIFF / `2` INVALID). Fixture validation runs first: a
  fixture that is not the verified fossil-positive capture, misses the census, or
  does not cover the required decoration halo is INVALID — never a green.
- `tests/fossil_chunk_acceptance_test.py` — 12 unittest cases: fixture integrity
  (fossil-positive proof, full-window census, halo coverage, provenance binding),
  comparator-bite probes (one planted difference per arm must be counted), and the
  end-to-end gate run (`--repeat 2`). Skips when `target/debug/megu` is not built,
  the same convention as `tests/client_launcher_test.py`.
- `tests/data/nether_fossil_4242_chunk_-22_20_1211.tsv.gz` — canonical-state TSV
  (25,336 non-air rows) produced by `tools/native_chunk_capture.py` from the
  frozen capture `nat-4242-nether-fossil-e43` at the dimension window `0..256`.
  `native_chunk_capture.py` gained additive `--min-y/--height` flags for this
  (the Nether preset window is `0..128`; the fixture covers the full `0..256`).
- `tests/data/nether_fossil_4242_chunk_-22_20_1211.meta.json` — provenance:
  pinned-jar SHA-1/SHA-256, `level.dat` + region + `order.tsv` + `run.log` +
  `boundary.json` SHA-256s, chunk census, all four saved heightmaps, the verified
  `structures.starts` child (`nefos`, template `nether_fossils/fossil_14`,
  `CLOCKWISE_180`, BB `[-356,55,319,-350,61,324]`), the 15 in-chunk
  `bone_block` cells, all 79 pending fluid ticks of the 9-chunk decorate area,
  and the second-run variability note.
- `tests/data/nether_fossil_4242_pop_order.tsv` — the recorded native population
  order (9 source chunks `(-23..-21, 19..21)`), the same bytes as the capture's
  `order.tsv` (SHA-256 `0f3b50d7…`).

The fixture filenames match the `/tests/data/**/*_1211*` `.gitignore` rule like
the other `*_1211*` parity fixtures; committing them needs `git add -f`, the same
way the existing tracked `*_1211*` fixtures got in.

## Measured result at HEAD

`python3 tools/fossil_chunk_gate.py --repeat 2` → **MATCH, exit 0**:

| arm | compared | differences |
|---|---|---|
| block states | 65,536 cells (25,336 non-air), canonical `name[props]` | **0** |
| heightmap | `OCEAN_FLOOR`, all 256 columns | **0** |
| pending ticks | 79 fluid ticks across the 9 populated chunks | **0** (0 port-only, 0 native-only) |

- `dump.bin` SHA-256 `b6628d89652caa1ba6cb1ffc66f2ea1781afea501f87e7e0f3edb9b64a1f0f90`
  — byte-identical to the 2026-11-20 measurement in
  `template-fossil-p1-20261120.md`, so the port still produces the same chunk.
- Determinism: `--repeat 2` requires byte-identical `dump.bin`/`canon.tsv`/
  `hm.bin`/`fluid_ticks.tsv` across runs; matched.
- Native variability: two independent frozen captures of the same seed/chunk
  (`matrix-p1/nat-4242-nether-fossil-e43` and `matrix-b5/nat-4242-b5-nether-fossil`,
  different saved worlds) contain **byte-identical** cells, identical
  `fluid_ticks`, and identical `OCEAN_FLOOR`. The fixture's 0-diff requirement is
  therefore grounded in measured reproducibility, not one lucky capture.
- `tests/fossil_chunk_acceptance_test.py`: **12/12 PASS in 8.3 s**.

## Gate-bites proof (temporary breaks, all restored)

| break | result |
|---|---|
| one fixture cell flipped `lava[level=0]`→`dirt` (in a `/tmp` copy, `--data`) | exit **1** `DIFF`, `states: 1 differing cells` |
| `pop_order.tsv` truncated to 8 lines | exit **2** `INVALID`, names the missing halo chunk `(-21,21)` |
| `MEGU_SEED` temporarily edited to `0` in `build_env` | exit **1** `DIFF`, `17,659 differing cells` + `+31 port-only / +79 native-only` pending ticks; restored → `MATCH` |

The `ComparatorBites` cases inside the test repeat this in-memory per arm
(wrong state, missing cell, heightmap column, dropped/extra pending tick).

## Oracle arms that remain open (the checklist box stays unchecked)

- **Runtime tick delivery + save/reload, light, wire**: explicitly separate gates
  per slice 04 — the capture is frozen at `structures` with `runtime_writes: 0`,
  so this gate measures generation, not delivery.
- **Block entities**: native recorded `0` for all 9 chunks; the offline dump emits
  no block-entity channel, so this arm is recorded-not-compared (reported in the
  JSON as `native: 0, port: "not emitted by the offline dump"`).
- **Block ticks**: native `block_ticks` = 0 everywhere; `MEGU_WG_FLUID_TICKS`
  emits fluid ticks only, so the port block-tick arm is also recorded-not-compared.
- **Heightmap coverage**: compared on `OCEAN_FLOOR` (the map
  `MEGU_WG_DUMP_HM` produces). `MOTION_BLOCKING{,_NO_LEAVES}` and
  `WORLD_SURFACE` are recorded in the meta but have no port dump producer; all
  four maps are uniform `128` in this chunk, so the arm is weak-but-pinned here.
- **Overworld `minecraft:fossil`** (the configured feature, not the Nether
  structure): `structure_capture.py --dry-run` already reports `no structure set
  lists minecraft:fossil` — a positive Overworld fossil chunk needs a different
  admission path and is uncaptured. This gate covers `minecraft:nether_fossil`,
  which is the fossil the P1 lane's structure-template work actually targets.
- **Second seed / second location**: one chunk, one seed. A second
  fossil-positive capture is a new capture job, not a code change.
- **`verify.sh` wiring**: intentionally not done here (the file is outside this
  task's scope). The test runs standalone:
  `python3 tests/fossil_chunk_acceptance_test.py` /
  `python3 tools/fossil_chunk_gate.py`.

## Reproduction

```bash
cargo build --offline --locked --bin megu
python3 tools/fossil_chunk_gate.py --repeat 2 --json-out report.json
python3 tests/fossil_chunk_acceptance_test.py -v
```

No jar, JDK, network, or native world is needed: the gate reads only committed
fixtures. Regenerating the fixtures needs the pinned jar's capture under
`~/mc-spine-local/captures/` and runs `tools/native_chunk_capture.py` plus the
metadata extractor described in the meta's `produced` field.
