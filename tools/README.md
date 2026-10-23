# tools

診断・固定具の道具。どれも「推測せずに測る」ためのもので、対象を1つに絞るか、
比較を成立させる条件を明示する役割を持つ。

## 固定具（参照 world と port の dump を作る）

| 道具 | 何をするか |
|---|---|
| `carver_fixture.sh <tag> <cave> <extra> <canyon> [n]` | carver だけを残した world を作り、port と block 単位で diff。`ctl` pack で carver を差し替え |
| `ore_fixture.sh <tag> <keep> [n]` | `ore_*`/`disk_*` のうち `<keep>` 以外の placement を `count: 0` にした world を作る（1 feature 隔離） |
| `vanilla_nofeat_pack.py <jar> <world> [--keep-carvers]` | biome の features と carvers を空にした pack を書く（地形だけの比較用） |
| `vanilla_region_blocks.py` | region ファイルから chunk を取り出す共通層 |

## 比較（両側を同じ土俵で読む）

| 道具 | 何をするか |
|---|---|
| `carver_diff.py <dump> <world> [--n --x0 --z0 --cells --tick]` | port の dump と保存 world を block state 単位で diff。層別（carver/deep/surface）、tick 由来（葉の distance）を分離、`--cells` で座標と名前、**全 chunk skip なら exit 2** |
| `ore_diff.py <dump> <world> <block> [...]` | **ブロック1種**のセル集合を比較（ours-only/vanilla-only＋クラスタサイズ＝「同数で形違い」と「セルが少ない」を区別）。参照が full でない chunk は両側とも除外し、比較ゼロなら失敗 |
| `world_diff.py <worldA> <worldB> [...]` | 保存 world 同士を比較＝**参照自身の実行間ノイズ床**を測る |

## port 側の計測（env で切る）

| 変数 | 何が出るか |
|---|---|
| `MC_SPINE_WG_DUMP/_N/_X/_Z` | block state の offline dump（比較の入力） |
| `MC_SPINE_NOFEAT` / `_NOCARVE` / `_NOAQ` | 層を止めて帰属を分ける |
| `MC_SPINE_FEAT_NBR=0` | 隣 chunk を decorate しない（順序の実験用） |
| `MC_SPINE_POP_REGION=x0,z0,x1,z1` | その領域を辞書順で decorate（順序の実験用） |
| `FEAT_TRACE` / `CARVE_TRACE` / `AQ_TRACE` | 1 feature / 1 chunk / 1 column の生ログ |
| `FSEED_TRACE` / `INSQ_TRACE` / `BIOMEF_TRACE` | feature の添字集合・in_square の生座標・biome 判定 |
| `FSEED_ARM=cx,cz,i,step` | その placed feature の**型付きドロー**（`DRAW n f=i,step nextInt(16)=…`） |
| `SIMPLE_TRACE` / `ORE_TRACE` / `TREE_TRACE` / `ORE_CELL_TRACE` | 個別 feature の試行・origin・書き込み数 |
| `PF_UNKNOWN` / `CF_MISSING` | **落ちた** placement modifier / configured feature の名前 |
| `MC_SPINE_POP_REGION` / `MC_SPINE_DFDUMP` / `MC_SPINE_HM_DUMP` / `MC_SPINE_BIOME_AT` / `MC_SPINE_BIOME_DUMP` | 密度値・ハイトマップ・biome の2定義の実測 |

## 参照側の計測（javaagent: `tools/carve-agent/`）

| プロパティ | 何が出るか |
|---|---|
| `-Ddim=<-64>` | overworld だけに絞る（nether/end は同じ chunk 座標・同じ seed で動くため必須） |
| `-Dfseedchunk=cx,cz` / `-Dfseedarm=i,step` | feature の添字ログ / 型付きドローのログ（`RDRAW`） |
| `-Dinsqchunk` / `-Dbiomefchunk` / `-Dorechunk` / `-Dtreechunk` | in_square・biome 判定・ore origin・tree origin |
| `-Dqbiome` / `-Dveindump` / `-Ddecoorder` | quart biome・OreVeinifier の3値・装飾順 |

計装の注意: `javap` の1行目で **class か interface か**を確かめる（`dyn` の default は
`INVOKEINTERFACE`、`ChunkAccess`(`duy`) は `INVOKEVIRTUAL`）。ローカル slot は
メソッドごとに確認（`eis.a_` は 4/5）。ログの上限を切ったら**上限を出力**する
（12 件だけ見て「全試行」と誤読した）。
