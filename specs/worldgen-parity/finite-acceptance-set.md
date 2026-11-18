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
captureのruntime_writes問題未解), `trial_chamber`(stronghold chunk内で部分), `jungle_temple`(MATCH —
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
| village-taiga | 宣言は検証済みだが比較**DIFF 1/884736** — 受入行にしない(§3) |
| jungle-temple | **MATCH**(0/98304、2026-11-18実測)。121 → 47(樹冠、vine lane)→ **0**(`ClampedNormalInt`の`f2i`切り捨て、`jungle-dripstone-random-offset-20261118.md`)。port dumpはnative chunkとbyte一致(`77964c22…a715bf`) |
| Overworld (3,−2) | 順序・mineshaft残差あり(population-order lane進行中) |
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
  three-slice boundのため**未実行**。village-taiga は `bbf51d1` 以降 **3 / 884,736** のままで
  受入行にしない(§3)。`ruined_portal` の§4-5(2つ目の独立native run)は依然**満たせない**。
  T0(`StructurePiece.placeBlock`のmark)は未landingのまま — `bbf51d1` が
  「22 SHAPE_CHECK_BLOCKSセルはいずれもreference mark setに無い」と実測して不landingを選んでおり、
  本laneのS01–S03の0はその状態で成立している。

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
