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
| Overworld | 1×1 at `(3,−2)` | 4242 | native A/B両参照残す。population順序実装後の目標行(現状は残差記録) |
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
captureのruntime_writes問題未解), `trial_chamber`(stronghold chunk内で部分), `jungle_temple`(HOLD中).

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
| fortress 0/256 | 比較は0差だがprovenance INVALID(runtime_writes 16) — ルール拡張待ち |
| village-taiga | captureにruntime_writes>0 — 分離・再capture必要 |
| jungle-temple | 121 HOLD |
| Overworld (3,−2) | 順序・mineshaft残差あり(population-order lane進行中) |
| forest 3×3・dimension_terrain 4件・(0,0)・stronghold・(−96,35) | committed gate緑(新SHAでの再検証は行ごとに必要) |
| ruined-portal | captureのみ — manifest行と比較が未実施 |
| biome境界行 | v2 |

## 改定規則

- 行の追加/除外/変更は、結果を見てからでなく**理由を先に**書いたrevisionとしてのみ。
- seedの付け替え・座標移動は受入失敗と同義(黙ってやらない)。
