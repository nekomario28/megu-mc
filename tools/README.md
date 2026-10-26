# tools

診断・固定具の道具。どれも「推測せずに測る」ためのもので、対象を1つに絞るか、
比較を成立させる条件を明示する役割を持つ。

## 固定具（参照 world と port の dump を作る）

| 道具 | 何をするか |
|---|---|
| `bytecode_probe.sh <class> [pattern]` | 固定した1.21.1 jar から1クラスの `javap -p -c` を出す。公式mappingsで難読名に引き、入れ子jarから取り出す。証拠ページの javap 引用はこれで再現する |
| `feature_type_coverage.py [--unimplemented] [--overworld]` | pack の configured-feature 型 × `p_cf` の腕 × placed feature の入れ子 × それを参照する biome を静的に数える。**到達されていない型を先に実装しない**ための計測 |
| `kelp_fixture.sh <cx> <cz> [n] [seed]` | 海洋 biome を spawn から離れた場所で forceload し、**正の kelp セルをゲートで要求**してから port と block 単位で diff（0 なら失敗）。forceload は起動後に送る |
| `fixture_base.sh [dir]` | 固定具の基点（`CARVER_FIXTURE_BASE`、既定 `/tmp/carverfresh`）を組む。jar を sha1 で固定し、worldgen に効く設定をここで明示する。基点が無いと固定具は空振りする |
| `carver_fixture.sh <tag> <cave> <extra> <canyon> [n]` | carver だけを残した world を作り、port と block 単位で diff。`ctl` pack で carver を差し替え |
| `ore_fixture.sh <tag> <keep> [n]` | `ore_*`/`disk_*` のうち `<keep>` 以外の placement を `count: 0` にした world を作る（1 feature 隔離） |
| `vanilla_nofeat_pack.py <jar> <world> [--keep-carvers]` | biome の features と carvers を空にした pack を書く（地形だけの比較用） |
| `vanilla_region_blocks.py` | region ファイルから chunk を取り出す共通層 |
| `vanilla_data/gen_vanilla_data.sh --jar <1.21.1 server jar> [--mappings DIR \| --fetch-fabric-mappings] [--accept-eula]` | 1.21.1 jar から `assets/block_states_1211.json`・`datapacks/vanilla/**` (worldgen と tags/block)・`datapacks/vanilla/biome_climate_tree.json`・`tests/data/biome_probe_1211.tsv`、`--accept-eula` 付きで `config_packets.bin` (`capture_config_packets.py` 経由) を生成する (全て `.gitignore`、新しい clone は `cargo test` の前に1回実行)。jar の sha1 と Fabric mapping の sha256 を固定し、公式 data generator の出力と突き合わせてから書く。Java 21 が必要。出力は決定的 |

## 比較（両側を同じ土俵で読む）

| 道具 | 何をするか |
|---|---|
| `carver_diff.py <dump> <world> [--n --x0 --z0 --cells --tick]` | port の dump と保存 world を block state 単位で diff。層別（carver/deep/surface）、tick 由来（葉の distance）を分離、`--cells` で座標と名前、**全 chunk skip なら exit 2** |
| `ore_diff.py <dump> <world> <block> [...]` | **ブロック1種**のセル集合を比較（ours-only/vanilla-only＋クラスタサイズ＝「同数で形違い」と「セルが少ない」を区別）。参照が full でない chunk は両側とも除外し、比較ゼロなら失敗 |
| `world_diff.py <worldA> <worldB> [...]` | 保存 world 同士を比較＝**参照自身の実行間ノイズ床**を測る |

## port 側の計測（env で切る）

| 変数 | 何が出るか |
|---|---|
| `MC_SPINE_WG_PROFILE=n` | `n×n` chunk を処理する**offline診断モード**。診断用envだけを付けて通常起動すると、サーバーがlistenし続けるので、先にこのモードを指定し、終了ログを確認する |
| `MC_SPINE_WG_DUMP/_N/_X/_Z` | offline mode 内で block state dump（比較の入力） |
| `MC_SPINE_NOFEAT` / `_NOCARVE` / `_NOAQ` | 層を止めて帰属を分ける |
| `MC_SPINE_FEAT_NBR=0` | 隣 chunk を decorate しない（順序の実験用） |
| `MC_SPINE_POP_REGION=x0,z0,x1,z1` | その領域を辞書順で decorate（順序の実験用） |
| `FEAT_TRACE` / `CARVE_TRACE` / `AQ_TRACE` | 1 feature / 1 chunk / 1 column の生ログ |
| `FSEED_TRACE` / `INSQ_TRACE` / `BIOMEF_TRACE` | feature の添字集合・in_square の生座標・biome 判定 |
| `FSEED_ARM=cx,cz,i,step` | その placed feature の**型付きドロー**（`DRAW n f=i,step nextInt(16)=…`） |
| `KELP_TRACE` | kelp の候補ごとに 基準 y・高さ予算・停止位置・水判定を出す（列ごとの参照比較用） |
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
