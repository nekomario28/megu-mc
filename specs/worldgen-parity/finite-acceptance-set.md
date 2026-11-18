# Worldgen 有限受入セット v1(凍結案 — 未承認)

Status: **APPROVED(凍結) — 2026-11-18、ユーザ承認済み**。
基点: `39edf3c`(2026-11-18)。この文書は「worldgen一致」の主張を**事前宣言された有限集合**に
限定する。結果を見てから対象を増減させない。追加・変更は明示的なrevisionのみ。

## なぜ有限か

全seed×全座標×全構造物の一致は検証も反証もできない。受入は
「宣言した集合の全行が、宣言した比較規則で一致する」ことのみを主張する。
集合外は**未検証**として残る(失敗ではない)。

## 宣言

### 1. Seed(3つ、固定)

| seed | 根拠 |
|---|---|
| `0` | 既存 committed fixture群(forest 3×3・Overworld (0,0) canonical gate・dimension_terrain End/Nether)のseed |
| `-1` | dimension_terrain fixture群(End 3×3 at (100,−103)・Nether 3×3 at (−65,32))のseed |
| `4242` | native capture行列(matrix-v1/v2)のseed。2つの独立native run(A/B)が残る唯一のseed |

### 2. 比較window(座標は絶対値で固定)

| dimension | window | seed | 種別 |
|---|---|---|---|
| Overworld | 3×3 at `(−64,32)` | 0 | 構造物off・runtime 700tick再現込み(feature-contracts) |
| Overworld | 1×1 at `(0,0)` | 0 | canonical state gate(32,518非air) |
| Overworld | 1×1 at `(3,−2)` | 4242 | **revision 1を参照** |
| Overworld | 1×1 at `(−96,35)` | 4242 | stronghold chunk(31,721セル・mineshaft/trial chamber同居) |
| Nether | 1×1 at `(−25,6)` | 4242 | fortress(0/128と0/256の両行) |
| Nether | 1×1 at `(−22,20)` | 4242 | nether_fossil(同上) |
| Nether | 1×1 at `(−34,1)` | 4242 | basalt discriminator(design §5.1、census記録) |
| End | 1×1 at `(65,45)` | 4242 | end_city(0/256。塔273セル・WORLD_SURFACE=140) |
| End | 3×3 at `(100,−103)` | −1 | dimension_terrain |
| Nether | 3×3 at `(−65,32)` | −1 | dimension_terrain |
| End | 1×1 at end-outer | 4242 | 全air対照行 |

Biome境界: 現状**専用行を持たない**。v1では「各windowに跨るbiome混合を暗黙に含む」とし、
明示的境界行(温度/湿度勾配にまたがる列)は **v2課題** とする(存在しないものを受入に数えない)。

### 3. 構造物リスト(受入対象。vanilla 1.21.1の全構造ではない)

受入必須(現時点でcapture/gateが存在): `mineshaft`, `stronghold`, `nether_fortress`,
`nether_fossil`, `end_city`, `ruined_portal`(captureのみでgate未), `village`(taiga,
**MATCH — 2026-11-18に3→1→0/884,736**。`expected_runtime_writes` count 5 (`writes_sha256 62fc3ddc…`) は独立captureで同一。残差1セル `(82,69,−500)` は P2-1-T0 + `Feature.markAboveForPostProcessing` + promotion限定 `BushBlock` 除去で閉じた), `trial_chamber`(stronghold chunk内で部分), `jungle_temple`(MATCH —
2026-11-18に121→**0**(47→0)。残差47はdripstone_cavesの地下グループで、原因は
`ClampedNormalInt`(`bpr`)の`f2i`切り捨てを`round()`で実装していたこと。樹冠/east-edge bandは残差0。
`evidence/gameplay/feature-parity/jungle-east-edge-vine-edge-shape-20261118.md`,
`evidence/gameplay/feature-parity/jungle-dripstone-random-offset-20261118.md`).

各々の受入条件は「window内の全セル一致」で、構造物単独のゲートではない。
**HOLD中・未解の行は受入から除外し、除外理由をこの表に書く**(紛失させない)。
未実装構造物はv1の受入対象外(一覧は implementation-checklist 参照)。

### 4. 比較規則(全行共通)

1. canonical state id(名前+全プロパティ)。air種別(`air`/`cave_air`/`void_air`)を区別する。
2. 宣言windowの全セル(§2のgrid)。min_y/heightは行ごとの宣言値(dimension window)。
3. `fixture_suite` v1 manifestの行として緑: frozen・`runtime_writes=0`(または宣言済みexpected集合)・
   stage一致・seed一致・入力sha256束縛・子の比較n*n・skip 0。
4. heightmapは保存WORLD_SURFACE/MOTION_BLOCKING対照で0不一致(end-city行の実績と同じ形式)。
5. 2つの独立native runが存在するseed(4242)では、A/B両方に0差であること。

### 5. 受入の宣言(この文書のGoal文)

> 上記§1–§4の全行が `fixture_suite` MATCH(または同等のcommitted gate緑)であり、
> かつ除外行が§3の理由付きで列挙されているとき、
> 「**seed 0/−1/4242 の宣言window集合で、worldgen出力は vanilla 1.21.1 と一致する**」
> と言う。それ以外の一般化(全seed・全座標・全構造物・runtime含む)は主張しない。

### 6. 現在の充足状況(2026-11-18時点、参考)

| 行 | 状態 |
|---|---|
| matrix-v2 4行(end-city/basalt/fossil/sculk) | **MATCH**(exit 0) |
| fortress 0/256 | **MATCH**(0/65536、`expected_runtime_writes` 宣言済み・検証済み) |
| fortress 0/128(matrix-v1) | **MATCH**(0/32768、同じ宣言。宣言値が両captureでbit一致) |
| village-taiga | **MATCH**(0/884736、2026-11-18実測、3→1→**0**)。`expected_runtime_writes` 宣言済み(count 5、`writes_sha256 62fc3ddc…`)。P2-1-T0 + `Feature.markAboveForPostProcessing` + promotion限定 `BushBlock` 除去で閉じた。3 capture (`nat-4242-village-taiga`, `nat-4242-village-marktrace`, `nat-taiga-mlight`) すべて 0/884,736・`OCEAN_FLOOR` 0列差、`manifest-promotable.json` に昇格済み(11→12行、exit 0)。対照: `--mode terrain` 76,736/884,736・seed 4243 185,879/884,736 |
| jungle-temple | **MATCH**(0/98304、2026-11-18実測)。121 → 47(樹冠、vine lane)→ **0**(`ClampedNormalInt`の`f2i`切り捨て、`jungle-dripstone-random-offset-20261118.md`)。port dumpはnative chunkとbyte一致(`77964c22…a715bf`) |
| Overworld (3,−2) | **MATCH(Revision 1の順序入力比較)** — order-A vs Native A 0/98,304・order-B vs Native B 0/98,304(2026-11-18 S01実測、負の対照: 既定順序30/98,304)。既定順序は名前つきメンバーでありbit一致しないことが文書化済み |
| forest 3×3・dimension_terrain 4件・(0,0)・stronghold・(−96,35) | committed gate緑(新SHAでの再検証は行ごとに必要) |
| ruined-portal | **MATCH**(0/98304)— ただし単一native run(§4-5は未充足) |
| end-outer(全air対照) | **MATCH**(0/32768、両側全air)— 被覆の主張はしない |
| biome境界行 | v2 |

**2026-11-18 の行ごとの実測(acceptance-rows レーン、source `d12adb5`)**

- **provenance規則の拡張**(`tools/fixture_suite.py`): 行が
  `inputs.expected_runtime_writes = {count, blocks, writes_sha256}` を宣言でき、MATCHには
  fixtureの`runtime_writes`・`wrote=true`行のcanonical state census・`run.log`のRUNTIME行
  (log前置きを除いてsortしたもの)のsha256の**3つすべて**が宣言値と一致することを要求する。
  1つでも欠けたら/1セルでも違えば INVALID。宣言の無い行の `runtime_writes == 0` default は不変。
  加えて全行で `boundary.json` の値と `run.log` の `wrote=true` 行数が一致することを要求する。
- **fortress**: `count 16, nether_brick_fence[east=true,north=true,south=true,waterlogged=false,west=true] × 16,
  writes_sha256 81cdbd85…`。matrix-v1(0/128)とmatrix-v2(0/256)の2つの独立captureで**同じhash**。
  16セル全てが保存worldに存在。0/128=0/32768、0/256=0/65536、OCEAN_FLOOR heightmap 0列差。
- **village-taiga**: 書き込みは fortress と**同じ決定的・生成中のclass**(2つの独立captureで
  payload hash一致 `62fc3ddc…`、書き込みsiteはchunk-gen task/生成中feature、5セル全てが保存
  worldに存在、`game_times [3,3]`)。ゆえに宣言は正当だが、比較は**DIFF 1/884736**
  (2026-11-18、promotion-footprint lane: 3→1、両captureで同一)。残る1セルは
  (82,69,−500) の `air`→`fern` で、nativeの`air`は`LevelChunk.postProcessGeneration`
  (`dvi.H`、RUNTIME `82 69 -500 state=0 wrote=true` の stack)が書いたもの — つまり
  nativeはこのセルを**markしており**、このportのmark集合にはその位置が無い
  (`MARK_TRACE=all` に該当行なし、`FWRITE 82 * -500` にdisk書き込みなし)。
  宣言した書き込みセル2つ((98,19,−512)/(99,19,−512) の `water[level=1]`→`air`)は
  promotion footprint((n+4)²)と water の empty-collision-shape アームで一致した。
  **受入行にはしない**(§3のgreenのみ追加の規律)。
- **capture自身のprovenance**: matrix-v2 fortress-256 / matrix-v1 fortress / end-outer の
  `inputs.sha256` は instrument を含めて現ツリーと一致する。ruined-portal と village-taiga は
  2026-11-16 capture で、`tools/carve-agent/{Agent,CarveLog}.java` と `native_fixture.sh` の
  3件が capture 後に編集済みのため再現できない(既知のF5)。両行の束縛(level.dat / region /
  dump / run.log)は検証済みだが、`stage: structures` は**記録された主張**であって再導出ではない。
- **ruined-portal**: 3×3ではなく1×1 `(6,7)`、window −64/384(boundary.json)。
  `chunk_triage` 0/98304、OCEAN_FLOOR heightmap 0列差。window内に `ruined_portal` start (6,7) と
  mineshaft corridor 2本。
- **end-outer**: `(10,10)` 0/128、0/32768。native も port も non_air=0(全air対照)。
  suiteは「比較セル数>0」を要求するので全air参照を拒否しない — MATCHだが被覆の主張はしない。
- **未充足/未実行**: §4-5(seed 4242のA/B両方に0差)は ruined-portal と end-outer では
  **満たせない**(host上のcaptureは各1 runのみ。`/tmp`の同名dirは同一runの複製でlevel.dat一致)。
  §4-4のうち WORLD_SURFACE/MOTION_BLOCKING の port対照は**未実行**(port側のheightmap dumpは
  OCEAN_FLOORのみ、src変更は本レーンの所有外)。matrix-v1 fossil 0/128 行、matrix-v1 end-city
  0/128 行は**未実行**(前者は本レーンの対象外、後者は§1のF1によりwindow外273セルで比較不能)。
  なお matrix-v1 stronghold capture は `runtime_writes 10` を記録しており、fixture_suite 行に
  する場合は同じ宣言が必要(本レーンでは行を宣言していない)。

**2026-11-18 P2-1受入lane(blueprint `p2-1-p2-3-blueprint-20261118T050548Z` の S01–S03、three-slice bound)**

source `798f645f4305421615389a11ad3398f152f0be47`(brief記載の40桁SHA `798f6459e2f5…` は本リポジトリに
存在しない — `798f645` に一致するobjectは1つだけで、それがこのcommit。`perf/worldgen-parallel` tip)。
binary sha256 `486435229adfb20525e5c7cbe8c8fefd84074ed52170133742990419d5cde23d`。
これは**新しいnative captureを作らない**受入: 既存captureを現HEADのbinaryで再生成して比較した。

- **S01 mineshaft — Overworld (3,−2) seed 4242(Revision 1の「順序を入力とする比較」)— 0/98,304 両方向**。
  native A `captures/native/mc-native-ow-seed4242-20261111/world` に対し `order-A.txt`
  (`scratch/current/snowy-4242-20261113/`)= **0 / 98,304**(port dump `d789a04aad7e04d4…`)、
  native B `…-seed4242b-20261111/world` に対し `order-B.txt` = **0 / 98,304**
  (port dump `48db2c95d6f5dcb5…`)。両側 non_air 32,281・cave_air 51 が一致、window内の
  mineshaft start (4,3) の3 BB(mscorridor#85/#185, msstairs#86)が全部比較対象に入っている。
  **対照(計測器が差を見えることの証明)**: 同じwindowを既定順序で比較すると **30 / 98,304**
  (`30 diorite -> granite`, y=19..22, `outside_structures | underground`)で、既存記録の30と一致。
  つまり0は順序入力が効いている結果であり、比較の空回りではない。
  ただし `order-A/B` は **構成したwitness**(観測されたscheduleではない、
  `audit/population-order-20261118-015835/notes/order-artifact-provenance.md` §B)であり、
  Revision 1が要求する「順序を入力として指定した比較」の入力ではあるが、順序自体のgateではない。
  この行はfixture_suite manifest行に**しない**(native captureに`boundary.json`がなく、pop-order入力も
  manifest schemaに無い)。
- **S02 jungle_pyramid — (74,−41) seed 4242 — 0/98,304(MATCH)**。
  port dump sha256 `77964c224c9eeddff4bd0cea6ccbfc559647f44003ee001cccd6f36b33a715bf` =
  §6既存記録の`77964c22…a715bf`と**byte一致**、両側 non_air 34,353。
  OCEAN_FLOOR heightmap の port対照 **0列差**(§4-4の一部をこの行で充足)。
  **suites v1の行に昇格**: `matrix-v2/manifest-promotable.json` に
  `ow-4242-structures-74--41--64-384` を追加(level.dat / `world/region/r.2.-2.mca` / dump.bin を
  sha256束縛、stage structures、runtime_writes 0)。suiteは **9 MATCH / 0 DIFF / 0 INVALID、exit 0**。
  **対照**: 同じwindowを seed 4243 で比較すると 16,502 / 98,304 — 比較は全セルを見ている。
- **S03 fortress — Nether (−25,6) seed 4242 — 0/128 と 0/256 の両行とも0**。
  0/128 (matrix-v1, `--height-window noise`) = **0 / 32,768**、dump `265d490f19a86f64…`;
  0/256 (matrix-v2, `--height-window dimension`) = **0 / 65,536**、dump `4df69f134515e18f…`。
  どちらのdumpもmanifestが束縛している既存artifactと**sha256完全一致** — つまり宣言済み
  `expected_runtime_writes`(16 cells, `nether_brick_fence[east=true,north=true,south=true,
  waterlogged=false,west=true]`, `writes_sha256 81cdbd85…`)を現HEADが再現している。
  **対照**: stageを`--mode terrain`に落とす(構造物を書かない)と **2,124 / 65,536** で、
  うち nether_bricks 979セル(→netherrack 508 / →air 471)が消える — fortress writerのセルが
  比較集合に入っていることの証明。
- **lane gates**: fixture_suite `manifest-promotable.json` **8 → 9 MATCH、exit 0**。
  seed-0 の64chunk identity dump(`MC_SPINE_WG_DUMP_N=8`, 25,165,824 B)=
  `670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def` **bit-identical**。
  serial suite **694 passed / 0 failed / 1 ignored**、`--test-threads=8` も
  **694 passed / 0 failed / 1 ignored、exit 0**(3回)。
  注: threads8のlog中に `test result: FAILED. … 694 filtered out` が1行あるが、これは
  `registry_tests::a_panicking_scene_fixture_leaves_nothing_to_restore` の**isolated child**が
  意図的にpanicした出力(`694 filtered out`がchild実行の証拠)で、親suiteの結果行ではない。
- **本laneで触っていない/残り**: S04–S07(stronghold / end_city / nether_fossil / ruined_portal)は
  three-slice boundのため**未実行**。village-taiga は本laneのbaseでは3 / 884,736だったが、
  並行のVillage-3 lane(`88d5ed7`)が**1 / 884,736**まで閉じた(fern 1セルは帰因記録済み・受入行には
  しない、§3)。`ruined_portal` の§4-5(2つ目の独立native run)は依然**満たせない**。
  T0(`StructurePiece.placeBlock`のmark)は未landingのまま — `bbf51d1` が
  「22 SHAPE_CHECK_BLOCKSセルはいずれもreference mark setに無い」と実測して不landingを選んでおり、
  本laneのS01–S03の0はその状態で成立している。

**2026-11-18 P2-1受入lane 第2便(同blueprintのS04–S07、`9a6169ba`基点)**

source `9a6169ba4bd1637540c82214dd2f79b1205ffeba`(`perf/worldgen-parallel` tip、dirty 0)。binary sha256
`5b56f0aba28f185f998f8354dba6eb59fbc351195512ab49fee42be071524a52`(debug、HEADから
`cargo build --offline --locked -j4`)。**新しいnative captureは作らない**受入: 既存captureを現HEADの
binaryで再生成して比較した。raw: `audit/p2-1-acceptance-s04-s07-20261118T130912Z/`。

- **S04 stronghold — Overworld (−96,35) seed 4242 — 0 / 98,304**。capture `matrix-v1/nat-4242-stronghold`
  に自分の `order.tsv`(3×3、対象chunkが中心)を入力にして **0 / 98,304**(port dump
  `98ae8d73e4e6f448…`、両側 non_air 30,605・cave_air 1,116、window内に stronghold/trial_chambers/
  mineshaft の3 start が同居)。**受入行に昇格**: `matrix-v2/manifest-promotable.json` の
  `ow-4242-structures--96-35--64-384`(level.dat / `world/region/r.-3.1.mca` / dump / run.log /
  order.tsv をsha256束縛、stage structures)。このcaptureは `boundary.json` が `runtime_writes 10` を
  記録するので **`inputs.expected_runtime_writes` を宣言**した(count 10 = `oak_fence` 4状態×2 +
  `air`×2、`writes_sha256 7cb3aa18…`)。suiteは capture 自身の `run.log` の `wrote=true` 10行に
  ついて個数・census・hash の3つが一致することを検証(`"verified": true`)。さらに10セル全部を
  native chunk と port dump で突き合わせて state-for-state 一致(`triage/s04-runtime-cells.txt`)。
  `run.log` の window内書き込み行 9,920行・unique 8,699セルも全て一致。
  **対照**: `--mode terrain`(構造物・feature・carverを書かない)= **13,837 / 98,304**
  (stronghold `sh*`/trial_chambers `jigsaw#`/mineshaft `mscorridor#` のBB内セルが消える)。
  **既定順序の対照は 0 / 98,304** — このwindowでは記録順序と既定順序が同じ結果を出し、順序入力は
  ここでは判別力を持たない(控えめに記録する。S01の(3,−2)が30差だったのとは性質が違う)。
  OCEAN_FLOOR heightmap 0列差。
- **S05 end_city — End (65,45) seed 4242 — 0/256行は 0 / 65,536、0/128行は 0 / 32,768(受入行にしない)**。
  0/256 (`matrix-v2/nat-4242-end-city-256`、order.tsv入力): **0 / 65,536**、dump `124c58c4baa2081b…`
  = 既存manifestの束縛hashとbyte一致、両側 non_air 14,977。`run.log` の window内書き込み
  6,921行・unique 6,660セルも全て一致。**対照**: seed 4243 = 11,563/65,536、`--mode terrain` =
  2,275/65,536(いずれも end_city `ecp#` のセルを含む)。OCEAN_FLOOR 0列差。
  0/128 (`matrix-v1/nat-4242-end-city`)は `--out-of-window report` で **0 / 32,768**、ただし
  **section Y=8 の273セル(purpur系・`WORLD_SURFACE`の最大は140)が比較窓の外**なので §4-2 を満たさず
  **行にしない**(この欠落が `WORLD_SURFACE_vs_non_air_top_mismatches 121` の正体 — 窓を切った
  ことの artifact であって port の不一致ではない)。
- **S06 nether_fossil — Nether (−22,20) seed 4242 — 0/256 と 0/128 の両行が0**。
  0/256 (`matrix-v2/nat-4242-nether-fossil-256`、order.tsv入力): **0 / 65,536**、dump
  `b6628d89652caa1b…` = 既存束縛hashとbyte一致、両側 non_air 24,688。23 uniqueセルの書き込み一致。
  0/128 (`matrix-v1/nat-4242-nether-fossil`): **0 / 32,768**、dump `b5448161bc6b795b…`。
  **受入行に昇格**: `nether-fossil-4242-structures--22-20--0-128`(level.dat / `world/DIM-1/region/
  r.-1.0.mca` / dump / run.log を束縛)。このcaptureの `order.tsv` は **(−24,20) 中心**で(−22,20)の
  3×3を覆わないため、この行には `inputs.order` を**宣言しない**(比較は既定順序で走らせた)。
  そのcapture自身の対象 (−24,20) は自分の order.tsv 入力で **0 / 32,768**(dump `4710ef2d…`)。
  **対照**: 0/256窓で seed 4243 = 21,372/65,536、`--mode terrain` = 5,306/65,536
  (nether_fossil `nefos#0` の骨blockセルを含む)。OCEAN_FLOOR 0列差。
- **S07 ruined_portal — Overworld (6,7) seed 4242 — 0 / 98,304**(window −64/384、order.tsv入力)。
  dump `2e4e3fedab3ba71f…` = 既存束縛hashとbyte一致、両側 non_air 40,396・cave_air 171、window内に
  `ruined_portal` start (6,7) と mineshaft `mscorridor#60/#61`。`run.log` の window内書き込み
  3,795行・unique 3,628セルも全て一致。**対照**: `--mode terrain` = 11,047/98,304、seed 4243 =
  23,907/98,304(いずれも `rupo#0` のセルを含む)。OCEAN_FLOOR 0列差。
  **§4-5(A/B両方)は依然満たせない** — host上の native run は1本だけで、`/tmp` の同名dirも同じrunの
  複製(level.dat一致)。よってこの行は「MATCH、ただし単一native run」のまま据え置く。
- **lane gates**: fixture_suite `manifest-promotable.json` は **9 → 11行**、**11 MATCH / 0 DIFF /
  0 INVALID、exit 0**。serial suite **698 passed / 0 failed / 1 ignored**(=699、exit 0)、
  `--test-threads=8` は 3回中 **2回 698/0/1 緑**、初回だけ
  `tick_tests::a_saved_snapshot_never_holds_an_unbooked_gravity_block` が
  `the saver must capture at least one snapshot` (src/tick_tests.rs:5303) で落ちた — saver thread が
  1度も走る前に `stop` が立つ**テスト側の scheduling race**で、本laneはsrcを1行も変えていない
  (serial では緑)。seed-0 の64chunk identity dump は `670fdfe3…9949def` で**bit-identical**。
  **mutation entry は追加なし** — 4スライスとも HEAD で既に0で、挙動を変える修正をlandしなかった。
- **§6上の記述の更新(本laneの実測が上書きする)**:
  1. 「matrix-v1 fossil 0/128 行は未実行」→ 本laneで **0/32,768**、受入行にした(上記)。
  2. 「matrix-v1 end-city 0/128 行は未実行(F1により比較不能)」→ 本laneで 0/32,768 を実測し、除外は
     **ちょうど273セル(section Y=8)**であることを確認。窓を広げるのが正しい(F1の記述は維持)。
  3. 「stronghold capture は runtime_writes 10 …本レーンでは行を宣言していない」→ 本laneで宣言した。
  4. 「fortress-256/fortress/end-outer の `inputs.sha256` は instrument を含めて現ツリーと一致する」→
     **9a6169b では成立しない**。`238da77`(2026-11-18 09:03)が `tools/carve-agent/Agent.java` と
     `CarveLog.java` を編集したため、それ以前の **matrix-v1 の `ow-c6x0-20261118` を除く全captureと
     matrix-v2 の end-city/fossil/basalt/fortress も** `Agent.java`/`CarveLog.java`/
     `build_carve_agent.sh`(+ matrix-v1 は `native_fixture.sh`) が一致しない(実測:
     `triage/provenance.txt`)。
     一致するのは `238da77` 以降の capture(jungle-tree/vine/vine-all、ow-c6x0)だけ。これは
     **capture instrument の再現性**の話で、比較の妥当性(world/level.dat/run.log をbyte束縛し、
     port と state-for-state で比べる)には影響しないが、「instrument を含めて現ツリーと一致」は
     F5の対象を ruined-portal/village-taiga に限らず**全 pre-238da77 capture に広げて読む**必要がある。
  5. **§2のwindow集合の外にあるmanifest行が2つある**: `ow-4242-structures-74--41--64-384`(前便が追加)
     と `ow-4242-structures-6-7--64-384`(ruined-portal)。§2は(6,7)も(74,−41)も宣言していないので、
     これらの行は §3 の構造物リストの主張(`jungle_temple`/`ruined_portal` の「MATCH」)を支えるが、
     §5のGoal文(§2のwindow集合上の量化)には入らない。§2は凍結なので、追加はrevisionで行う。

**2026-11-18 P2-1-T0 + village-taiga 収束(S08、基点 `354128a`)**

- **P2-1-T0(`StructurePiece.placeBlock` `SHAPE_CHECK_BLOCKS` mark配線)**:
  `src/template_structures.rs`(`TemplateWorld::mark_shape`、および `FC` 実装 `self.shape_marks.push(pos)` を `place_in_world` の変換済み state 書き込み後で呼び出し)と `src/jigsaw_structures.rs`(`JigsawTarget::mark_shape` を `place_single_or_legacy` の `target.set_world` 後で呼び出し)を接続した。`TestWorld` と `JigsawTarget::Scene` は no-op のため既存 oracle は不変。
- **`Feature.markAboveForPostProcessing`(`ece.a`) + promotion限定 `BushBlock` 除去**:
  pinned jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) + official mappings (`03f8985492bda0afc0898465341eb0acef35f570`) を `javap -p -c` で確認し、`MarkSrc::FeatureAbove`(`"above"`, `src/worldgen.rs`)と `mark_above_for_post_processing`(`y+1`/`y+2` を最初の air または build height 外で停止、`src/feature.rs`)を `Cf::Disk` と `Cf::Lake`(`y >= 4` の `cave_air` 書き込み時と `barrier` 書き込み時の2箇所、`<4` の fluid では呼ばない)から呼び出した。`postprocess_bush_shape`(`src/template_structures.rs`)は `BushBlock` 階層(25単体種: `TallGrassBlock`, 非wither 13小花, 非azalea/mangrove 7苗木, `pink_petals`, `sweet_berry_bush`)および `DoublePlantBlock` 6種を `postprocess_shape_marks` のみに限定して適用した(共有 `update_shape` には入れない)。
- **S08 village-taiga — Overworld `(4,−32)` 3×3 seed 4242 — `1 → 0 / 884,736`(MATCH)**:
  - `matrix-v1/nat-4242-village-taiga`(recorded order `03d15e34…`): **0 / 884,736**、`OCEAN_FLOOR` 0列差、port dump `0dd5a9d8abb350c3d73adaa573aa083a128b8eaa75ef3a5d696618a1d11654e8`。
  - `matrix-v2/nat-4242-village-marktrace`(recorded order `5f2eea53…`): **0 / 884,736**、`OCEAN_FLOOR` 0列差、port dump `0dd5a9d8abb350c3…`(両captureで同一)。`MARK_TRACE=all` 上で `(82,69,−500)` が `chunk=5,-32 src=above` としてマークされ、promotion により `air` へ書き換わることを確認。
  - `t6b/nat-taiga-mlight`(recorded order `d1b02f30…`): **0 / 884,736**、`OCEAN_FLOOR` 0列差。
  - **対照**: `--mode terrain` = **76,736 / 884,736**、wrong-seed `4243` = **185,879 / 884,736**。
  - **`manifest-promotable.json` 昇格**: `ow-4242-structures-4--32--64-384-n3` を追加し **11 → 12行、12 MATCH / 0 DIFF / 0 INVALID、exit 0** (`expected_runtime_writes`: count 5 = `air`×3 + `water[level=1]`×2、`writes_sha256 62fc3ddcc136e96cad1e45fc197a744fbe7f807d0a47db5ae2ecdda09b7cdde1`、両独立captureで同一)。なお(6,7)・(74,−41)と同様に `(4,−32)` 3×3 も凍結済み §2 には含まれないため §3 の `village` = MATCH と `manifest-promotable.json` を支える行であり、§2 の追加は別 revision に委ねる。
  - **seed-0 64-chunk identity dump(`MC_SPINE_WG_DUMP_N=8`)**: `670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def`(25,165,824 B) **bit-identical**。詳細は `evidence/gameplay/feature-parity/village-taiga-p2-1-t0-and-mark-above-closure-20261118.md`。

## 改定規則

- 行の追加/除外/変更は、結果を見てからでなく**理由を先に**書いたrevisionとしてのみ。
- seedの付け替え・座標移動は受入失敗と同義(黙ってやらない)。

## Revision 1(2026-11-18、測定に基づく変更)

**対象**: §2のOverworld `(3,−2)` 行の受入条件。

**理由(結果後だが証拠に強制された)**:
[population-order-is-a-discipline-20261118.md](../../evidence/gameplay/feature-parity/population-order-is-a-discipline-20261118.md)
がjar実読と4probe実測で示したとおり、vanillaのpopulation順序は**level優先の規律**であり、
level内順序はwall clock由来で**vanilla自身もrun-to-runで再現しない**(同一設定のp1/p2で共通先頭9/49のみ)。
ゆえに「既定順序で両参照に0差」はいかなる実装でも達成不能(参照AとB自身が23セル異なる)。

**改訂後の受入条件**: この行の一致は **順序を入力として指定した比較**で判定する。
`MC_SPINE_POP_ORDER=order-A.txt` で native A に、`order-B.txt` で native B に、
それぞれore差分0(population順序由来の差がないこと)。既定順序自体は
「規律の名前つきメンバー(level優先+宣言済みtie-break)」として実装し、
どのcaptureともbit一致しないことを明記する。順序が入力であることは
corpusの性質であり、既定順序の欠陥ではない。
