# E4-5 `sculk_patch` parity investigation (2026-11-20)

**P2-2 remains open for default worldgen.** The saved native reference still
differs from the freshly built default port dump by 324 sculk-family cells.
Replaying the **native population pass order** with the same port binary reduces
that comparison to **zero**, passing `--require-match`. This isolates the
observed default-dump residual to source-chunk population selection/order,
outside `src/feature.rs` ownership; it does not establish default-worldgen parity.

## Reproduction and attribution

- Branch: `t3/fix-sculk-patch-parity`; starting HEAD:
  `9a1cc1303cddaf5f8ebbdfcaf49a1b975c433d21`.
- Pinned 1.21.1 server: `/home/yuu/tmpx/carverfresh/server.jar`, SHA-1
  `59353fb40c36d304f2035d51e7d6e6baa98dc05c`.
- Native source: `/var/tmp/chunkfix-gate/world`, saved reference from
  2026-10-29; seed 0, chunks `(-16,112)` through `(-15,113)`.
- Native pass trace: `/var/tmp/sculk-trace-mk16.txt` (130 recorded source
  passes). Replay plan was extracted by
  `python3 tools/sculk_trace.py pop-order /var/tmp/sculk-trace-mk16.txt --out target/tmp/native-pop-order.tsv`.
- The two runs used the **same** freshly rebuilt `target/debug/mc-spine` from
  `cargo build --offline --locked`, `MC_SPINE_WG_PROFILE=0`,
  `MC_SPINE_SEED=0`, `MC_SPINE_WG_DUMP_N=2`, `MC_SPINE_WG_DUMP_X=-16`,
  `MC_SPINE_WG_DUMP_Z=112` and separate `MC_SPINE_WG_DUMP` output paths.
  All commands used `TMPDIR="$PWD/target/tmp"`.

| Port generation | Native sculk / vein / catalyst | Port sculk / vein / catalyst | Missing | Extra | Wrong-state | `--require-match` |
|---|---|---|---:|---:|---:|---|
| Default, before edit | 175 / 125 / 1 | 265 / 131 / 1 | 107 | 203 | 14 | FAIL (exit 1) |
| Default, after edit | 175 / 125 / 1 | 265 / 131 / 1 | 107 | 203 | 14 | FAIL (exit 1) |
| Native-order replay, after edit | 175 / 125 / 1 | 175 / 125 / 1 | **0** | **0** | **0** | **PASS (exit 0)** |

Reproduction with the final default dump:

```sh
python3 tools/sculk_gate.py /var/tmp/chunkfix-gate/world \
  target/tmp/final-default.bin 2 -16 112 --require-match
```

Replay adds `MC_SPINE_POP_ORDER="$PWD/target/tmp/native-pop-order.tsv"`
and `MC_SPINE_POP_ORDER_SCOPE=recorded` to the same dump invocation; the gate
is then run against `target/tmp/final-replay.bin`. The replay executed 130
source chunks. The pass-order selection is implemented by the diagnostic
population-plan path in `src/main.rs`, which is outside this lane's edit scope.
These results isolate the default mismatch to the population context supplied
to the feature, rather than a remaining disagreement in the sculk spreader's
output when those recorded inputs are aligned. The saved reference is not a
fresh capture and the replay is an explicit diagnostic mode.

## In-scope correction

Pinned-jar `SculkPatchFeature` (`edi.a(ecg)`) rejects flowing water at the
origin (`FluidState.isSource`) and requires a neighbouring block with a full
collision shape (`BlockState.isCollisionShapeFullBlock`). The previous port
used any water block and a face-sturdy neighbour. Updated
`spc_can_spread_from` in `src/feature.rs` to match those native eligibility
conditions. Added deterministic tests for dirt path versus a full-collision
stone neighbour, and for flowing water versus source water. This correction
does not change the four-chunk default capture because these cases do not
account for its population-order mismatch.

## Verification

| Check | Result |
|---|---|
| `cargo build --offline --locked` | PASS |
| `cargo test --offline --locked -- sculk --test-threads=1` | PASS, 18 passed / 0 failed |
| `cargo check --offline --locked --tests` | PASS |
| `TMPDIR="$PWD/target/tmp" python3 tests/sculk_gate_test.py` | PASS, 12 tests |
| `bash tools/layers_check.sh` | PASS, 44 modules / 111 edges / 0 named errors |
| `bash tools/debt_ratchet.sh` | PASS, 0 over limit |
| `python3 tools/provenance_check.py` | PASS, clean |
| `git diff --check` | PASS |

The replay gate proves exact sculk-family state parity in these four chunks
under the recorded pass sequence. **P2-2 cannot be closed for normal generation**
until the default population planning/order is reconciled with native.
