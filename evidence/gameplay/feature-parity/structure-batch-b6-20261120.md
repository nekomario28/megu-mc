# Phase E Wave 4 / E4-6 Batch B6 structure capture and full-state triage (2026-11-20)

## Frozen reference and comparison method

- Port checkout: branch `t3/b6-structure-capture-triage`, HEAD `9a1cc1303cddaf5f8ebbdfcaf49a1b975c433d21`. `cargo build --offline --locked` **PASS** (12.45 s). Rebuilt `target/debug/mc-spine` SHA-256 `07d33d53742e687d64d9c4fb87f6285a497306ba142db583bd01ba2c7d7605b0`. `tools/structure_capture.py` SHA-256 `9e91e2a062f75e4d1524b18d324231caa14d193ebcc441ddd1d7df16eef52712`.
- Native authority: pinned Vanilla 1.21.1 `<home>/tmpx/carverfresh/server.jar` SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`; `/usr/lib/jvm/java-21-openjdk` (`openjdk 21.0.13.1`). Seed `4242`, frozen `structures` stage. Nether used `dimension` window Y `0..255` = **65,536** typed cells/chunk; Overworld used `noise` window Y `-64..319` = **98,304** typed cells/chunk.
- Temp policy: `mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"`; Java ran with `JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData"`. All captures, dumps, triage JSON, native logs, refuted dirs and gate logs live **outside Git** in `<local>/captures/matrix-b6/`.
- Every capture's saved `structures.starts` and `Children[]` were verified before triage. Each comparison covered the whole 16×16 chunk grid including air kinds and all block-state properties. `port_triage.py` ran `--mode full`, `noaq=False`, with no excluded attributes. Canonical grids are 262,144 bytes (Nether) / 393,216 bytes (Overworld).
- This lane **measures and classifies only**; no row was promoted into `matrix-v2/manifest-promotable.json`, no Rust code was changed, and no gate was weakened.

## Measured rows

`0` = PROMOTED (exact full-state equality); `>0` = HOLD with the mismatching cells attributed below. `NO VERDICT` means no clean native capture was obtained, so no diff was measured.

| Structure | Dimension, chunk `(cx,cz)` | Verified saved `Children[]` | Example native child `BB` | Full typed-cell diff | Verdict |
|---|---|---|---|---:|---|
| `minecraft:bastion_remnant` | Nether `(-104, -124)` | 4 | `[-1664,32,-2013,-1617,55,-1984]` | **0 / 65,536** | **PROMOTED** |
| `minecraft:mansion` | Overworld `(15, -133)` | 4 | `[238,62,-2121,258,80,-2106]` | **690 / 98,304** | **HOLD** |
| `minecraft:pillager_outpost` | Overworld — | — | — | no admitted candidate | **NO VERDICT** |
| `minecraft:trial_chambers` | Overworld `(-225, -254)` | 4 | `[-3618,-31,-4082,-3600,-12,-4064]` | **0 / 98,304** | **PROMOTED** |
| `minecraft:ancient_city` (intro-list extra) | Overworld — | — | — | 4 candidates refuted | **NO VERDICT** |

Zero-diff captures had equal native/port non-air counts (bastion `23,833`, trial chambers `29,823`), zero heightmap/air-projection differences, and zero skipped chunks. The native `run.log` for each measured row is bound by hash below.

### HOLD detail — `minecraft:mansion`, 690 cells

All 690 differences fall in **terrain around the mansion**, not inside a mansion piece BB; the mansion's own 24 in-window `wmp` pieces are unchanged. Grouped by native → port type (counts sum to 690):

| Native → port | Count | Bounding coordinates `(x, y, z)` | Example cells |
|---|---:|---|---|
| `minecraft:cobblestone` → `minecraft:water[level=0]` | 447 | `x=240..255`, `y=50..61`, `z=-2128..-2113` (527-cell underground bucket + `wmp#0/#38/#272/#274/#326/#327/#329` halos) | `[254,50,-2114]`, `[240,61,-2122]`, `[246,61,-2128]`, `[253,61,-2128]` |
| `minecraft:dirt` → `minecraft:sand` | 131 | `x=240..252`, `y=56..61`, `z=-2121..-2117` | `[241,57,-2121]`, `[240,57,-2120]`, `[252,61,-2121]` |
| `minecraft:dirt` → `minecraft:gravel` | 111 | `x=240..243`, `y=56`, `z=-2124..-2120` | `[241,56,-2124]`, `[240,56,-2123]`, `[243,56,-2120]` |
| `minecraft:air` → `minecraft:moss_carpet` | 1 | `[255,8,-2123]`, biome `minecraft:lush_caves` | `[255,8,-2123]` |

Attribution buckets: `outside_structures | underground` 527 (`dark_forest` 526, `lush_caves` 1) and seven `near_structure_halo(d=1) | minecraft:mansion | minecraft:wmp#…` buckets totaling 163. Native-A-vs-B noise is 0, so the differences are deterministic, not run-to-run jitter. The **first divergent terrain/water writer is UNKNOWN**; the measured cells do not justify a change to the owned structure files, so the row stays HOLD.

### No-verdict rows

- `pillager_outpost`: the port admitted no candidate within the requested 256-chunk radius (`structure_capture` exit 2, exact message: `no admitted candidate for minecraft:pillager_outpost (seed 4242) within 256 chunks of (0, 0)`). No capture, no comparison. This matches the earlier B4 observation at 192 chunks.
- `ancient_city` (run to cover the intro's list; `--manifest .../manifest-ancient-city.json`): 467 admitted candidates, but the biome hint reported that no candidate's container biome was in `ancient_city`'s biome set. The first four admission-order captures — `nat-4242-ancient_city--236--250`, `…-210--251-try2`, `…-168--250-try3`, `…-81--255-try4` — each had empty `starts` and were refuted (`refuted.txt` kept). `structure_capture` exit 2: `every candidate was refuted … No comparison was made.` No diff or verdict.

## Artifact SHA-256 hashes

The grid hashes bind the complete canonical grid; `order.tsv` binds the population-order input; `run.log`/`level.dat`/region bind the native authority. Native and port grid hashes are equal for every PROMOTED row.

| Row | `native.bin` = port `dump.bin` | `order.tsv` | `run.log` | `level.dat` |
|---|---|---|---|---|
| bastion_remnant | `2aeb8cfb8a8b51e378b0c1bb3a21914cfb8900455ee06fe70448b78fcfe88ea2` | `d34a88f846a6807c8e248c6ff87dd890f21c57cb298b6fad433c701bc94997c4` | `2887d605521813f0f928bf57499ec8fe89b7ad5db766399a34fa959c07803244` | `d4ee84e213be788eb2c703b1231bed58af1653d2d194947ccb7768bbda04acca` |
| mansion | `eac9f6f20342ca7777a5bf8eb4066d90811c16e4c0002badb0c14f6ca1963159` / `ac01a97ad2f2bde9a8ba1b0ae714a6b17266b33a40b2bd12c83764a83e348680` | `78118a214f547c93ed3b5c1f5b68e9fbd204b00f19a90cf358560899043b39c0` | `06b1306ff697b211b95987d1251acf4eaec8873e6527aabebb8dbd971de9a6b0` | `5ddd5ede09fd2dba23cf0ccff16638c4a586b7f30a80e0e2e082c9a933628c9e` |
| trial_chambers | `ede83e59234642d7f4e9c58f509d8477ecb91ec1ae5e0790a6e5b9ddcb143a82` | `21ac95743133ca743dfdd9f77779a02987f7d93c4bf7429ee14de54349ba1177` | `86d5cb4bf92f18eb17c6e31ac9cc4b9c16490bd6bd626ce5dcf959d35a8f1e09` | `8e3351a97a1718d99502090a2723e03603a464caaa1849ec8a6a306a85d579d2` |

Native saved dimension region hashes: bastion `DIM-1/region/r.-1.-1.mca` `23a22ca5c7bac3992aa21c5c6cfadc5abbb2e1e32d6339a665c86b9a06b6604d` (plus `-1.0`, `0.-1`, `0.0`); mansion `region/r.0.-4.mca` `90d899962bb9647a0e8cbe7f9aab6c4ff59dc491d659a50eb63c7494eab5706d` (plus `-1.-1`, `-1.0`, `0.-1`, `0.-5`, `0.0`); trial chambers `region/r.-8.-9.mca` `58dce4a1661484d109e3bf7ba8989f5de817e8a744500bb4eae97a07a099e915` (plus six others). Full per-file hashes are in `matrix-b6/b6-evidence-extract.txt`.

## Commands, exit codes, and verification

| Command | Exact observed result |
|---|---|
| `cargo build --offline --locked` | **PASS**, `Finished \`dev\` profile ... in 12.45s`, exit 0 |
| `structure_capture.py --structure minecraft:bastion_remnant --dimension nether --scan-radius 128` | exit 0; `port vs native: 0 differing cell(s) / 65536`; row `nether-4242-structures--104--124-0-256` |
| `structure_capture.py --structure minecraft:mansion --dimension overworld --scan-radius 256 --max-attempts 4` | exit 1 (diff present); `port vs native: 690 differing cell(s) / 98304`; row `ow-4242-structures-15--133--64-384` |
| `structure_capture.py --structure minecraft:pillager_outpost --dimension overworld --scan-radius 256` | exit 2; no admitted candidate |
| `structure_capture.py --structure minecraft:trial_chambers --dimension overworld --scan-radius 256` | exit 0; `port vs native: 0 differing cell(s) / 98304`; row `ow-4242-structures--225--254--64-384` |
| `structure_capture.py --structure minecraft:ancient_city --dimension overworld --scan-radius 256 --manifest .../manifest-ancient-city.json` | exit 2; all four candidates refuted; no row |
| `bash tools/layers_check.sh` | **PASS**, `source graph: 44 modules, 111 edges, 0 named errors` / `layers check: PASS (current debt is named, not resolved)`, exit 0 |
| `bash tools/debt_ratchet.sh` | **PASS**, `wall-clock reads: 0`, `ignored tests: 1`, `debt ratchet: 0 over limit`, exit 0 |
| `python3 tools/provenance_check.py` | **PASS**, `provenance: clean - none of assets/pblocks.json, assets/properties.json, resourcepack.zip is tracked or referenced`, exit 0 |

Gate and capture logs are in `matrix-b6/{layers-check,debt-ratchet,provenance-check,b6-*}.log`; the Overworld `matrix-b6/manifest.json` holds exactly the 3 emitted rows.

## Scope boundary

These results establish exact parity only for the two zero-diff frozen chunk windows at the `structures` stage, and a deterministic 690-cell terrain HOLD for the measured mansion window. They do **not** verify other structure locations, block entities, loot, mobs, or later gameplay stages. Pillager outpost and ancient city remain **UNMEASURED** at this seed/radius, not promoted and not refuted as broken.
