# `P2-2 / sculk_patch` gate status at current HEAD `5759cbe` (2026-11-20)

A factual receipt for Phase E Wave 3 Lane E3-10. It records the exact gate
commands and output on the current HEAD. **Status: NOT PASS** — the acceptance
condition `tools/sculk_gate.py --require-match` still exits **1**.

## Identity / provenance

| item | value |
|---|---|
| git HEAD (full) | `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9` |
| branch | `t3/m3-sculk-gate-receipts` |
| `tools/sculk_gate.py` SHA-256 | `b3ee239b8681fee16ba0adc8adf271d90746c050ee308687ca2b0708db669732` |
| `tests/sculk_gate_test.py` SHA-256 | `390a064252f44f6888815b6018174dbbe4a23d472fac1160ccf206be9b92e2f3` |
| benchmark/port binary | `target/release/mc-spine`, SHA-256 `67be74097ce106dab497be3cd873f4162651b1468a7b7f32bb86668309b08076`, built from this HEAD |
| gate test result (task 1) | `TMPDIR="$PWD/target/tmp" python3 tests/sculk_gate_test.py` → `Ran 12 tests in 0.698s / OK` (exit 0) |
| Python | `Python 3.14.10` |

## Gate contract (`tools/sculk_gate.py --help`)

The gate compares **only** the sculk family (`sculk`, `sculk_vein`,
`sculk_catalyst`, `sculk_shrieker`) between a saved vanilla reference region and
an existing port dump, using canonical block-state ids (not just block names).
It captures nothing itself; a region with no sculk is an error, not a pass.
Usage: `tools/sculk_gate.py <region_world> <dump.bin> <n> <x0> <z0> [--require-match] [--json-out PATH]`.
Exit codes: `0` cells agree (or report-only), `1` a sculk cell disagrees with
`--require-match`, `2` invalid/unreadable input. The root-cause note in
`evidence/gameplay/feature-parity/native-oracle.md` explains why the
feature-level oracle cannot drive `sculk_patch` (a scripted level cannot answer
the `ChunkAccess` shape update), so this whole-chunk gate is the acceptance route.

## Inputs bound to the measurement

- **Saved native reference region** (read-only; captured 2026-10-29, not
  regenerated here): `/var/tmp/chunkfix-gate/world`, with region files
  `r.-1.3.mca` (SHA-256 `ff3c6692cdb46014c524653e8298528405b9e493717c61580f819e8483d94c51`),
  `r.-1.-1.mca` (`946deb3521e755a271c44c28156f30aa78e9b2cc4b5d74c96930c23863ba4974`),
  `r.-1.0.mca` (`f451723b2683580389388977cc0318859130c173e7237cb7046fa19222ce4343`),
  `r.0.-1.mca` (`127a73b2a328381c5478f4493ef6abde82c3f9fc8604790c648ccb9851bad4bd`),
  `r.0.0.mca` (`a5aaeca4062b6440afaad6209e6c73fe99d16a163e1bcb4211e09b0dd78396c9`).
  This repo's reader uses 32x32-chunk region files (`cx >> 5`), so chunks
  (-16,112)…(-15,113) live in `r.-1.3.mca`.
- **Current-HEAD port dump** (freshly generated for this receipt, in scratch
  `target/tmp`, then removed): `MC_SPINE_WG_PROFILE=0`,
  `MC_SPINE_WG_DUMP=$PWD/target/tmp/head-sculk.bin`, `MC_SPINE_WG_DUMP_N=2`,
  `MC_SPINE_WG_DUMP_X=-16`, `MC_SPINE_WG_DUMP_Z=112`, `MC_SPINE_SEED=0`,
  `FEATURE_ATTEMPT_TRACE=1`, `SPC_CURSOR_TRACE=1`, `./target/release/mc-spine`
  → exit 0, 1,572,864 bytes, SHA-256
  `c409803db1e93f0b52fbf88d409bb909f0eed7b4a67c43e809c7de1d2888b08d`.
- All commands used `mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"`.

## Current-HEAD gate run (fresh dump vs saved native reference)

Exact command:

```
TMPDIR="$PWD/target/tmp" python3 tools/sculk_gate.py \
    /var/tmp/chunkfix-gate/world target/tmp/head-sculk.bin \
    2 -16 112 --require-match \
    --json-out target/tmp/head-sculk-gate.json
```

Exit code **1**. Exact printed output:

```
chunks compared: 4/4
reference sculk cells: {'sculk': 175, 'sculk_vein': 125, 'sculk_catalyst': 1} total 301
port sculk cells:      {'sculk': 265, 'sculk_vein': 131, 'sculk_catalyst': 1} total 397
missing 107  extra 203  wrong-state 14
  missing: -242,-45,1823 vanilla sculk port deepslate
  missing: -244,-44,1823 vanilla sculk port deepslate
  missing: -243,-44,1823 vanilla sculk port deepslate
  missing: -242,-44,1823 vanilla sculk_vein port air
  missing: -241,-44,1823 vanilla sculk_vein port air
  missing: -244,-43,1821 vanilla sculk port deepslate
  missing: -243,-43,1821 vanilla sculk port deepslate
  missing: -245,-43,1822 vanilla sculk port deepslate
  extra: -249,-40,1821 vanilla air port sculk_vein
  extra: -248,-39,1820 vanilla air port sculk_vein
  extra: -250,-39,1821 vanilla deepslate port sculk
  extra: -249,-38,1820 vanilla deepslate port sculk
  extra: -248,-38,1820 vanilla air port sculk_vein
  extra: -250,-38,1823 vanilla air port sculk_vein
  extra: -241,-37,1821 vanilla air port sculk_vein
  extra: -251,-37,1823 vanilla tuff port sculk
  wrong-state: -249,-40,1822 vanilla sculk_vein#22862 port sculk_vein#22926
  wrong-state: -249,-40,1823 vanilla sculk_vein#22926 port sculk_vein#22862
  wrong-state: -249,-39,1821 vanilla sculk_vein#22926 port sculk_vein#22911
  wrong-state: -248,-37,1820 vanilla sculk_vein#22910 port sculk_vein#22926
  wrong-state: -249,-37,1821 vanilla sculk_vein#22911 port sculk_vein#22910
  wrong-state: -250,-37,1822 vanilla sculk_vein#22926 port sculk_vein#22910
  wrong-state: -252,-35,1823 vanilla sculk_vein#22926 port sculk_vein#22910
  wrong-state: -241,-32,1821 vanilla sculk_vein#22923 port sculk_vein#22907
sculk gate: FAIL - 324 sculk cells disagree
```

`--json-out` counts: `chunks_compared=4/4`, `reference.total=301`, `port.total=397`,
`counts={missing:107, extra:203, wrong_state:14, diffs:324}`, `exit_code=1`.

### Remaining gap (exact)

| | sculk | sculk_vein | sculk_catalyst | total |
|---|---|---|---|---|
| reference (native, saved) | 175 | 125 | 1 | **301** |
| port (current HEAD) | 265 | 131 | 1 | **397** |

`missing 107` (vanilla placed sculk/vein, port did not) + `extra 203` (port placed
them where vanilla did not) + `wrong-state 14` (both placed sculk but a different
canonical state, all `sculk_vein` property mismatches) = **324 disagreeing cells**,
gate **FAIL / exit 1**. The shape of the failure is unchanged from the recorded
history (`specs/worldgen-parity/sculk-patch.md`): the port is sculk-heavy and
vein-light to place, and `sculk_vein` `up/down/...`+`waterlogged` state ids do not
line up. `P2-2` therefore remains **open**; `specs/implementation-checklist.md`
still lists "P2-2 残るconfigured featuresと実chunk差分" as unchecked for
`sculk_patch`, and the recorded acceptance test (`sculk_gate.py --require-match`
exit 0) is not met.

## Cross-checks on previously saved pairs (read-only, no regeneration)

1. **Saved deep-dark dump** `/var/tmp/chunkfix-gate/dump.bin` (SHA-256
   `087bb0bbe708b6ebb0ba64d7b2ceec9521736115f9480ff920a9061b48448d79`)
   against the same reference region, `n=2 x0=-16 z0=112 --require-match`:

   ```
   chunks compared: 4/4
   reference sculk cells: {'sculk': 175, 'sculk_vein': 125, 'sculk_catalyst': 1} total 301
   port sculk cells:      {'sculk': 348, 'sculk_vein': 78, 'sculk_catalyst': 1} total 427
   missing 130  extra 256  wrong-state 15
   sculk gate: FAIL - 401 sculk cells disagree
   ```
   exit 1 — same failure shape; `wrong-state` is 15 here vs the older name-based
   record of 1 because the current gate compares canonical state ids, not names.

2. **Small 1-chunk fixture pair** from the fixture-suite demo
   (`<local>/audit/fixture-suite-20261116-204353/demo/captures/sculk/capture/world`
   + `port.bin`, seed 4242, chunk (0,0), `n=1 x0=0 z0=0 --require-match`):

   ```
   chunks compared: 1/1
   reference sculk cells: {'sculk_vein': 4} total 4
   port sculk cells:      {'sculk_vein': 4} total 4
   missing 0  extra 0  wrong-state 0
   sculk gate: PASS
   ```
   exit 0. This shows the gate can PASS when a pair actually matches, but its
   scope is only 4 `sculk_vein` cells in one seed-4242 chunk — it does **not**
   exercise the deep-dark `sculk_patch` placement/spread path, so it is not a
   P2-2 PASS.

## Task 4 gates on this exact HEAD

```
$ bash tools/layers_check.sh
gate_probe.py: [cycle] a: dependency cycle: a → b → a
gate_probe.py: [forbidden-dependency] lower: 'lower' must not depend on 'upper'
gate_probe.py: [layer-violation] lower: 'lower' (layer 1) depends on 'upper' (layer 0) — the dependency points upward
gate_probe.py: [cycle] upper: dependency cycle: upper → lower → upper
source graph: 44 modules, 111 edges, 0 named errors
layers check: PASS (current debt is named, not resolved)
```
exit 0.

```
$ bash tools/debt_ratchet.sh
ok    wall-clock reads (clock RNG must not return): 0
ok    ignored tests (parked gates): 1

debt ratchet: 0 over limit
```
exit 0.

```
$ python3 tools/provenance_check.py
provenance: clean - none of assets/pblocks.json, assets/properties.json, resourcepack.zip is tracked or referenced
```
exit 0.

## What this receipt does and does not establish

- It establishes that at HEAD `5759cbe` the current-HEAD port dump, compared
  against the saved 4-chunk deep-dark reference region, **fails** the sculk gate
  with 324 disagreements (107 missing / 203 extra / 14 wrong-state), exit 1.
- `sculk_patch` is **not** at PASS, and P2-2 remains open. The gate's own scope
  limit applies: only sculk-family cells in this saved region, not whole-world
  parity, and the reference region is a 2026-10-29 capture (read-only here),
  not freshly recaptured at this HEAD.
- The 4-cell fixture PASS is a tool sanity check, not a sculk_patch parity claim.
