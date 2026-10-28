# tools

診断・固定具の道具。どれも「推測せずに測る」ためのもので、対象を1つに絞るか、
比較を成立させる条件を明示する役割を持つ。

## 固定具（参照 world と port の dump を作る）

| 道具 | 何をするか |
|---|---|
| `vanilla_src.sh <official class...>` | 固定1.21.1 jar を**ソースに逆コンパイル**して読む（`javap` はオフセット引用、こちらはアルゴリズムの移植）。公式mappingsで難読名に引き、入れ子jarから本体と入れ子クラスを出す。jar と mappings の SHA-1 を検査し、**repo 内への出力は拒否**する（Mojang のコードは読解用で、再配布しない）。既定の出力は `~/.cache/mc-spine-work/mc121/src` |
| `structure-oracle/NativeHarness.java` + `FeatureOracle.java` + `feature_oracles.py [<type> --cases <json> [--write] [--cells <path>]]` | 固定 jar の**実 feature クラス**を scripted level で実行し、`scene/oy/seed/wrote/cells/digest/rngProbe` を記録する。cases はデータ（origin 相対の ply・seeds・box）、`class` は registry と一致を assert。port 側は同じ cases を `generic_feature_cases` が読む。失敗時は `GENERIC_CASE_DUMP=<path>` で port セルを出して cell 単位で diff する。契約と**測った限界**（署名済み jar で `ChunkGenerator` を継承できない／`getLevel()` は供給不能／build height 外の scene は不可）は [native oracle の記録](../evidence/gameplay/feature-parity/native-oracle.md) |
| `bytecode_probe.sh <class> [pattern]` | 固定した1.21.1 jar から1クラスの `javap -p -c` を出す。公式mappingsで難読名に引き、入れ子jarから取り出す。証拠ページの javap 引用はこれで再現する |
| `feature_cases/gen.py <type> [--support <block>] [--force]` | 型の cases skeleton を pack と `tests/data/feature_classes_1211.tsv` から生成する（難読クラス名と config は機械的、scene は定型7種）。**型固有の判別 scene は手で足す**（そこが型の本体）。生成物は `tests/feature_cases/<type>.json` |
| `feature_type_coverage.py [--unimplemented] [--overworld]` | pack の configured-feature 型 × `p_cf` の腕 × placed feature の入れ子 × それを参照する biome を静的に数える。**到達されていない型を先に実装しない**ための計測 |
| `kelp_fixture.sh <cx> <cz> [n] [seed]` | 海洋 biome を spawn から離れた場所で forceload し、**正の kelp セルをゲートで要求**してから port と block 単位で diff（0 なら失敗）。forceload は起動後に送る |
| `geode_fixture.sh [cx] [cz] [n] [seed]` | geode を含む vanilla 参照を作る（既定 seed 12 / chunk (0,0) は port の配置 trace で geode が発火することを実測済み）。参照に geode が無ければ失敗し、amethyst_block / budding_amethyst / calcite / smooth_basalt をセル単位で比較する |
| `biome_tree_from_jar.py --jar <server.jar> [--output <tree.json>]` | 固定1.21.1 jar の built-in preset を実クラスに構築させ、気候 RTree を抽出。JDK必須。別jar・不完全な木・jarに無いbiomeを拒否し、成功時だけ出力を置換する。jarとexporterのhashを記録 |
| `container_oracle.py [--jar <server.jar>] [--check <fixture> \| --output <fixture>]` | 固定1.21.1の `Slot.safeInsert` / `ChestMenu.quickMoveStack` を実行する小型oracle。既定は記録済みfixtureとの完全一致検査。別jar・空／途中の出力・参照の欠落は失敗する。検証範囲は[チェストの証拠](../evidence/gameplay/containers/README.md)を参照 |
| `inventory_oracle.py [--jar <server.jar>] [--check <fixture> \| --output <fixture>]` | 固定1.21.1の `Inventory.add` / merge・空き検索を実行する。入力／参照の完全性はcontainerと同じ検査を使う。Playerは生成を迂回した実ServerPlayerをabilities読み出しにだけ使い、entity/worldの実行とはしない。[範囲と再現](../evidence/gameplay/inventory-pickup/README.md)を参照 |
| `surface_scan_oracle.py` | 実1.21.1の`SurfaceSystem.buildSurface`を合成stone/water chunkで実行し、scanのgoldセルを照合する。world/server実行ではない。[範囲](../evidence/foundation/surface-context/README.md)を参照 |
| `surface_condition_oracle.py` | 実1.21.1の`SurfaceRules`条件をsynthetic contextで評価し、stone/water/hole/prelimの入力と結果を全件照合する。chunk/world実行ではない。[範囲](../evidence/foundation/surface-context/README.md)を参照 |
| `surface_oracle.py [--jar <server.jar>] [--check <fixture> \| --output <fixture>]` | 固定1.21.1の `SurfaceRules.RuleSource.CODEC` / `BlockState.CODEC` / `Biome` の温度計算を呼ぶ。既定は入力・結果・provenance全体の照合。world／surface contextの実行とは区別する。[範囲と再現](../evidence/foundation/surface-loader/README.md)を参照 |
| `packet_angles_oracle.py` | 固定1.21.1の実 spawn/teleport constructor と head量子化 helper を呼び、角度 byte の入力・結果・provenance を照合する。意図した記録のみ `--output`。[範囲と再現](../evidence/gameplay/entity-angles/README.md)を参照 |
| `primitives_oracle.py` | 実1.21.1のpalette書き出し・StringTag・有限AABB clipを検査。既定は全入力・結果・provenanceを照合し、意図した記録だけ `--output`。worldや任意NBT decoderの検証とは分ける。[範囲](../evidence/foundation/valence-primitives/README.md)を参照 |
| `biome_carver_oracle.py` | 固定1.21.1 jar の datapack から実 `Biome`／`BiomeGenerationSettings` の AIR carver 列を取得し、order込みで照合する。既定は全biomeの完全一致、意図した記録だけ `--output`。[範囲](../evidence/foundation/biome-carvers/README.md)を参照 |
| `aquifer_oracle.py` | 固定1.21.1 jar の datapack を `RegistryDataLoader` に読ませ、実 `NoiseChunk`／`Aquifer` の preliminary surface と chunk 内の流体状態を照合する。既定は入力・結果・出自の全件一致、意図した記録だけ `--output`。[範囲](../evidence/foundation/aquifer/README.md)を参照 |
| `biome_spawn_oracle.py` | 固定1.21.1の実 `Biome.getMobSettings()` からspawners全カテゴリ・weight・spawn_costs・creature probabilityを取得し、全biomeを照合する。意図した記録だけ `--output`。[範囲](../evidence/foundation/biome-spawners/README.md)を参照 |
| `biome_tie_oracle.py` | 固定1.21.1の実 `Climate` 木で、コミット済みのtie-heavy biome点を全件再検索する。thread-localのprevious結果を毎回消してtie順を固定。[範囲](../evidence/foundation/upstream-prs/README.md)を参照 |
| `nextint_oracle.py` | 固定1.21.1の実 `Xoroshiro128PlusPlusRandom`／`WorldgenRandom`／`LegacyRandomSource` で、コミット済み `nextInt(bound)` 列を全行再計算する。jar認証後に実行し、参照値は変更しない。[範囲](../evidence/foundation/pr-integration/README.md)を参照 |
| `weird_sampler_oracle.py` | 固定1.21.1の実mapper・NormalNoise・WeirdScaledSamplerを呼び、入力／raw f64結果／出自を全件照合する。world全体の検証とは分ける。[範囲と再現](../evidence/foundation/weird-sampler/README.md)を参照 |
| `vanilla_pack_from_jar.sh [dir]` | 同じ固定jarの worldgen JSON・block tag・built-in biome tree をまとめて更新する（JDK必須） |
| `fixture_base.sh [dir]` | 固定具の基点（`CARVER_FIXTURE_BASE`、既定 `/tmp/carverfresh`）を組む。jar を sha1 で固定し、worldgen に効く設定をここで明示する。基点が無いと固定具は空振りする |
| `carver_fixture.sh <tag> <cave> <extra> <canyon> [n]` | carver だけを残した world を作り、port と block 単位で diff。`ctl` pack で carver を差し替え |
| `vanilla_sculk_pack.py <jar> <world> --variant veins\|one-patch [--count N] [--height MIN:MAX]` | deep-dark の step 7 を片方だけに絞った datapack を書く（`sculk_vein` と `sculk_patch_deep_dark` は 同じ decorator stream を共有するので、全セル比較ではどちらが動いたか言えない）。port 側にも同じ ファイルを入れる必要がある（script は path を印字するだけで port の pack には触らない）。 fixture 側は `sculk_veins` / `sculk_one_patch` / `sculk_cave_patch` モード |
| `tools/carve-agent/`（`Agent.java` + `CarveLog.java` + `SculkLog.java`、`JAVA_AGENT=` で参照サーバーに載せる） | 固定 jar を ASM で instrument して**native の中間状態**を取る。sculk 用に `SCURSOR r=<round> i=<index> x y z charge decayDelay updateDelay facingMask`（`dmp$a` の `ChargeCursor.update` 入口）と `SCMARK`（呼び出し元）/`SSPREADALL types=<n>`（spread type 数）を出す。`-Dfeaturebox=minX,minY,minZ,maxX,maxY,maxZ` で feature 層の書き込みを `FCELL x y z state=<id> feature=<gi>,<step> chunk=<pop>` としても出る。port 側は対応する `SPC_CURSOR_TRACE` / `SPC_TRACE_ALL` / `FEATURE_WRITE_TRACE`（+`GENERIC_CASE_DUMP`）で同じ形を出せる。[計器と実測](../specs/worldgen-parity/sculk-patch.md) |
| `build_carve_agent.sh [out-dir]` | carve-agent の jar を組んで**検証する**（`Agent.class`/ASM/ロガーの中身、マーカー文字列、フックの配線まで確認してから返す）。手順を手で打つと `head` が javac の終了コードを飲んだり、半分適用された編集から jar を作ったりして「動いているように見える古い jar」ができる（実測で2回踏んだ）。`sculk_capture.sh` はこれを使う |
| `sculk_capture.sh <tag> [--chunk CX CZ] [--n N] [--mode M] [--jar PATH] [--settle S]` | **native capture を1コマンドに**する。agent を組む → fixture を回す（trace は `-Dsculktrace=` で自前ファイルへ）→ `PROVENANCE-<tag>.txt`（seed/chunk/agent sha/行数）を書く → `sculk_trace.py sanity` で**読めない trace を不合格**にする。既定の保存先 `~/.cache/mc-spine-audit/sculk-traces/` |
| `sculk_trace.py compare-draws <native> <port-draw.log>` | port の draw 列を native の流れから **`(bound, value)`** で探し、一致長と最初の食い違いを出す（native 側は `SculkLog.drawIBound` が bound を出す）。実測: `sculk_patch_deep_dark` chunk (-16,112) で **24本一致**、25本目で port `nextInt(16)=12` / native `nextInt(49)=6` |
| `sculk_trace.py summary\|sanity\|passes\|windows\|draws\|names\|compare-passes\|extract\|patches` | trace の読み取り・検査・比較を**1つの道具**にした。`sanity` は「窓に draw が入っていない」「`SPLACE` が feature 表に無いクラスを指す」「`run.log`（= log4j、行順がプログラム順でない）を位置で切ろうとしている」を**不合格にする**。実測: 健全な capture = SANE、`ebq`（=`ConfiguredFeature`）で arm した capture = UNUSABLE、draw フック無し = UNUSABLE。`compare-passes` は pass 単位で試行 origin 集合を突き合わせ、差の分布と最初の食い違いを出す |
| `sculk_trace.py extract <run.log> [--origin x,y,z] [--lines N]` | `SculkSpreader$ChargeCursor.update` の trace を抽出し **(round, index) で整列**して比較する（cell の集合ではなく trajectory を比べる）。共有キーが食い違えば exit 1。native 側の先頭スライスは `tests/data/sculk_cursor_1211.tsv`。capture はパッチの出現順と `r` の通し番号が実行ごとに変わるので、`--origin` でパッチを選び `r` をパッチ内 round に正規化する（実測: 同一パッチは 2 capture で 60/60 一致）|
| `sculk_gate.py <world> <dump.bin> <n> <x0> <z0> [--require-match]` | sculk ファミリ（`sculk`/`sculk_vein`/`sculk_catalyst`/`sculk_shrieker`）のセルだけを参照 region と port dump で照合する。feature 単体の oracle が使えない `sculk_patch` の合格条件。sculk の無い参照は exit 2（何も証明しない）。[経路と実測値](../specs/worldgen-parity/sculk-patch.md) |
| `chunk_world_fixture.sh <tag> <cx> <cz> <n> <terrain\|carvers\|features\|structures>` | 固定jarの同一座標regionをforceloadし、mc-spineの同じstage dumpと`carver_diff`で比較する。参照chunkの完成を先に検査する |
| `ore_fixture.sh <tag> <keep> [n]` | `ore_*`/`disk_*` のうち `<keep>` 以外の placement を `count: 0` にした world を作る（1 feature 隔離） |
| `vanilla_nofeat_pack.py <jar> <world> [--keep-carvers]` | biome の features と carvers を空にした pack を書く（地形だけの比較用） |
| `vanilla_region_blocks.py` | region ファイルから chunk を取り出す共通層 |
| `vanilla_data/gen_vanilla_data.sh --jar <1.21.1 server jar> [--mappings DIR \| --fetch-fabric-mappings] [--accept-eula]` | 1.21.1 jar から `assets/block_states_1211.json`・`datapacks/vanilla/**` (worldgen と tags/block)・`datapacks/vanilla/biome_climate_tree.json`・`tests/data/biome_probe_1211.tsv`・`biome_probe_ties_1211.tsv`（range 端の同距離 tie 点。子の走査順を固定）・`random_next_int_1211.tsv`（乱数クラスの `nextInt(bound)` 列のハッシュ、数値のみ）、`--accept-eula` 付きで `config_packets.bin` (`capture_config_packets.py` 経由) を生成する (全て `.gitignore`、新しい clone は `cargo test` の前に1回実行)。jar の sha1 と Fabric mapping の sha256 を固定し、公式 data generator の出力と突き合わせてから書く。Java 21 が必要。出力は決定的 |

## 比較（両側を同じ土俵で読む）

| 道具 | 何をするか |
|---|---|
| `carver_diff.py <dump> <world> [--n --x0 --z0 --cells --tick]` | port の dump と保存 world を block state 単位で diff。層別（carver/deep/surface）、tick 由来（葉の distance）を分離、`--cells` で座標と名前、**全 chunk skip なら exit 2** |
| `ore_diff.py <dump> <world> <block> [...]` | **ブロック1種**のセル集合を比較（ours-only/vanilla-only＋クラスタサイズ＝「同数で形違い」と「セルが少ない」を区別）。参照が full でない chunk は両側とも除外し、比較ゼロなら失敗 |
| `world_diff.py <worldA> <worldB> [...]` | 保存 world 同士を比較＝**参照自身の実行間ノイズ床**を測る |
| `heightmap_diff.py <world> <cx> <cz> <port_hm.bin> [--n] [--heightmap]` | 保存 chunk の heightmap と port の dump を列ごとに比較（内訳: 領域外縁/内部、chunk内 lz/lx 別）。保存値は床からの相対値なので変換して比べる。**保存 heightmap が最終ブロックと一致しない列があり、現状の数字は「port が違う」証拠にはならない**（carvers §62.2b） |

## port 側の計測（env で切る）

| 変数 | 何が出るか |
|---|---|
| `MC_SPINE_WG_PROFILE=n` | `n×n` chunk を処理する**offline診断モード**。診断用envだけを付けて通常起動すると、サーバーがlistenし続けるので、先にこのモードを指定し、終了ログを確認する |
| `MC_SPINE_WG_DUMP/_N/_X/_Z` | offline mode 内で block state dump（比較の入力）。**hot region の外では返らない**（実測: spawn から172 chunk で wall 240s / user 0.3s = 計算せず待っている、79 chunk なら5s）。遠い領域の固定具はこれを使えないので、先に `populate` の待ちを診断するか、領域を hot region 内に取る |
| `MC_SPINE_WG_DUMP_HM=<path>` | 同じ領域の各列の OCEAN_FLOOR 高さ（絶対y、256×i32/chunk）を書く。`heightmap_diff.py` の参照と対にする |
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

## sculk trace の読み方（この順で）

1. `tools/sculk_capture.sh <tag>` — 組んで・回して・`sanity` で弾く。trace の行順は
   **プログラム順**（`SculkLog` が自前の writer に書く）。`run.log` は log4j 経由で、同じ
   スレッドの行順すら保たない（実測: ある窓に draw が1本も無いのに、同じスレッドの stream には
   41208 本ある）ので、**位置で切る解析を `run.log` に対してやってはいけない**
   （`sculk_trace.py` はそれを拒否する）。
2. `tools/sculk_trace.py summary <trace>` — 種類ごとの件数、窓のうち draw を持つものの数、
   `SPLACE` に出てくるクラスと feature 型（`tests/data/feature_classes_1211.tsv` 経由）。
3. `tools/sculk_trace.py sanity <trace>` — 読めない trace を不合格にする。特に
   「draw はあるのに位置らしい draw（小さな `nextInt`）が窓に無い」は、**arm 点が1段深い**
   （`ebq`=`ConfiguredFeature.place` ではなく `eiv.b`=`PlacedFeature.placeWithBiomeCheck`）
   という形で、これを一度踏んでいる（下の spec ページ）。
4. `tools/sculk_trace.py compare-passes <native> <port>` — pass 単位の突き合わせ。
   `--arm leading|trailing` は agent の arm 位置で決まる（`sanity` がどちらかを報告する）。

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
