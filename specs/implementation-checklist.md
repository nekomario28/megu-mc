# 実装・検証チェックリスト（`perf/worldgen-parallel`）

作成: 2026-11-06。基点: `48dfe73b4e1db64d5ce295f0e533d30780a8fc37`。
更新: 2026-11-19（同期基準: `e82fbc6`）。状態: **部分実装・限定検証済み。上位P0〜P4の完了は別判定**。
作成時点の履歴と、下記の現行棚卸しを混同しない。

この文書は**作業順、担当する既存owner、完了ゲートを一覧化する実行台帳**。
目標の正本は [PLAN](PLAN.md)、順序と出口条件の正本は [ROADMAP](ROADMAP.md)。
詳細な意味論は [worldgen-parity](worldgen-parity/README.md)・
[template-finalization](template-finalization/README.md)・
[chunk-parallelism](chunk-parallelism/README.md)、実測値は `evidence/` を正本とし、
ここに数値や歴史記録を複製しない。

## 判断と運用

- 対象は **Minecraft Java 1.21.1 / protocol 767**、固定公式jar `59353fb40c36d304f2035d51e7d6e6baa98dc05c`。
- 計画のみのDraft PRと34ファイルの未コミットbatchは作成時点の前提であり、現在の着手条件ではない。最初に実際のHEAD・index・worktree・remoteを確認し、他作業を上書き・stash・restoreしない。push・統合・公開/releaseにはその都度の指示と検証範囲の確認が必要。
- 各項目は **native実クラスの同一入力 → Rust独立比較 → 本番経路で再実行 → 故障注入（compiling mutant）→ 広い回帰** の順で閉じる。fixtureの0件実行・skip・old captureを「一致」に数えない。
- 同じseed、dimension、datapack、structures設定、source順、margin、chunk FULL、時計、生成stageを固定。**生成停止（paused）とruntime tick後を別々**に比較し、全block state、書き込み順、RNG、NBT、pending/delivered tick、保存/再読込、wireを混同しない。
- **ステータスは実際に検証した項目だけ `[x]`** にする。`tests/verify.sh`、native JAR比較、GitHub Actions、第三者レビューは別々に記録する。ホストCI未起動は成功ではない。
- 既に確認済みの限定契約（forest 3×3の旧checkpoint、template processor・fossil・proto tickのnative fixtures）は消さずに回帰として維持する。新HEADでの全体合格には置き換えない。

## 現行の棚卸し（2026-11-13、上位項目の一括完了ではない）

| 項目 | 現在ある限定契約 | 残す出口 |
|---|---|---|
| P0-1 | [ローカル全gate checkpoint](../evidence/gameplay/feature-parity/verification-20261112.md)がある | 次の挙動変更の対象SHAで再検証。hosted CI・独立レビューとは別 |
| P0-2 / P0-4 | seed 0のcanonical-state gateと、seed 4242の探索captureがある | 2つ目のseedの再現契約・stage/writer帰属。未知入力とskipの到達性監査 |
| P1-0 / P1-1 | tree/templateの共通面順と限定neighbor/support比較基盤 | 全contextの汎用neighbor chain、予約から実効果まで |
| P1-2 | supported container Items・sign front textを既存storeへstaging。entity offset変換も比較済み | 実entity生成、任意NBT、loot/finalizer、実Levelを要する受け入れ |
| P1-3 | proto-to-level handoffと限定callback順 | chunk activation、実block/fluid効果、native保存/再開 |
| P1-4 | 1つのOverworld chunkの全state比較 | 正のFossil実chunk、heightmap/NBT/pending tick、runtime・wire/light |
| P2〜P4 | 個別helper・dimension terrainの比較はある | 全構造物・通常dimension・M2/M3・資産出所・独立レビュー・リリース判断 |

ソース所有者と境界は [template finalization](template-finalization/README.md)、
詳細は各sliceを参照。表の「ある」は上位チェックを `[x]` にする根拠ではない。
測定結果は証拠ページ、次の順序は [ROADMAP](ROADMAP.md)、実行方法は
[因果差分の調査手順](worldgen-parity/investigation-workflow.md)に置く。

## 優先順・依存関係・完了条件

### P0 — 作業の検証基盤と入力意味論（並行して先行）

- [ ] **P0-1 HEAD再現と失敗の基線固定** — これから検証する対象SHAのコード・入力資産・固定jarのdigestを紐付け、`tests/verify.sh`（serial / 8-thread×3 / wire / adapter / layers / debt / mutations）で現在の一次失敗を収集。テスト失敗をmutex poisonやCI課金停止と混同しない。各結果に実行環境・終了コード・省略項目を記録する。
- [ ] **P0-2 1.21.1 density/surface/biomeの到達性監査** — `01a6528` で `tools/p0_2_audit.py` の `Cond::Biome(u128)` アンカー検証・`< 128` 上限同期および 3 次元到達性監査スナップショット（[`evidence/foundation/audit-snapshots/p0-2-reachability-20261119.md`](../evidence/foundation/audit-snapshots/p0-2-reachability-20261119.md)、未対応・欠落ゼロ）を記録済み。残る `spawn_costs` と NaturalSpawner 実行経路の実クラス対照で完全閉鎖する。
- [x] **P0-3 再配布・出所の隔離判断（統合/公開ブロッカー — Stage F1 完了）** — `762f312` で D1〜D4、`d45cb4c` で D5、`88573be` で `GPL-3.0-or-later`（寄稿は `LGPL-3.0-or-later` とのデュアルライセンス）および全ファイルの出所確定を完了済み：
  - [x] **D1/D2 (`F1-1`, `762f312`)**: `tools/vanilla_data/gen_vanilla_data.sh`（`--verify` 2,350 ファイル・SHA-256 照合）と `assets/block_states_1211.json` の jar 自前生成化を完了。
  - [x] **D3/D4 (`F1-2`, `762f312`)**: Pumpkin (GPL-3.0) 同値の旧 `assets/pblocks.json`・`assets/properties.json` および出所不明の `resourcepack.zip` を消費者切替の上で削除し、`tools/provenance_check.py`（`--self-test` 13/13 PASS、`--strict-public` 対応）をゲート追加。
  - [x] **D5 (`F1-3`, `d45cb4c`, `106a107`, `88573be`)**: `tools/export_public_branch.sh`・`tests/public_export_test.py` 整備、不要な直下 `*.storyboard.png` 6枚の削除、`datapacks/{modtest,rectest,spinetest,tagtest,testpack}` の自作確定、および `LICENSE` (`GPL-3.0-or-later`) / `COPYING.LESSER` (`LGPL-3.0-or-later`) / `Cargo.toml` / `NOTICE` / `THIRD_PARTY.md` / `README.md` / `CONTRIBUTING.md` / `LICENSING.md` の完全統一を完了。
- [ ] **P0-4 native比較契約の固定** — Fossilが**正に配置される**fixture、complete FULL chunks、必要halo、sourceの生成順、2回のnative採取の再現性を定義。artifactに入力・SHA・対象セル数・除外数・pending tick・writerを記録し、古い測定値の流用を防ぐ。

### P1 — Template finalization（**最優先の実装列**）

- [x] **P1-0 完了済みの限定基礎（再回帰が必要）** — template/tree共通 `src/voxel_faces.rs` の面走査、液体予約順、`src/scheduled_ticks.rs` のchunkごとの生成tick unpack、bounded full-cube Fossilのnativeオラクル。**これらは最終state・実配送・一般構造物の合格ではない**。
- [ ] **P1-1 隣接stateの最終確定** — [仕様](template-finalization/slices/01-neighbors.md)。既存 `src/structure_template.rs` / `src/tree.rs` / runtime ownerを確認し、`updateShapeAtEdge` と `updateFromNeighbourShapes` の変更セルと順序・予約tickをnativeと比較。waterlogged・connected・fragile・falling・mixed fluid、full-cubeでもshape更新を起こす例を含む。新しい第2の状態機械を作らず単一ownerへ統合。未知呼び出しはfail closed。
- [ ] **P1-2 Block Entity / Entity初期化** — [仕様](template-finalization/slices/02-initialization.md)。NBT付きtemplate・entityを使いnativeの受理/拒否、回転、座標/UUID、loot seed、finalizerを観察。 `src/structure_template.rs` と既存runtime NBT/保存ownerを接続し、**実在する初期化済みインスタンス**とpacket・保存/別プロセス再読込を比較。既存のNBT無し許容経路は新契約が通るまで維持し、保存形式の変更はこの段で行わない。
- [ ] **P1-3 生成tickの実行・配送** — [仕様](template-finalization/slices/03-delivery.md)。protoからruntimeへの受け渡しだけでなく、native `LevelChunkTicks` / `LevelTicks` とゲーム時刻・priority・sub-order・cross-chunk・chunk eligibility・上限の**実際に実行されたイベント列と最終状態**を比較。block/fluidを独立→統合、保存/再開と未ロードchunkを含む。既存`scheduled_ticks`とGEN-lock-free drainを保つ。
- [ ] **P1-4 実チャンクFossil受け入れ** — [仕様](template-finalization/slices/04-chunks.md)。実際にFossilが置かれるnativeの完全chunkを採取し、同一stage・source順・marginで全state、heightmap、block entity、pending tickを比較。続けてruntime tickとsave/reload、light・wireは**別のgate**で評価する。scripted-level oracleの緑だけで閉じない。

### P2 — 全WorldGenへ段階的拡張

- [ ] **P2-1 汎用template / processor / structure starts** — [仕様](worldgen-parity/README.md)。**34定義・16アルゴリズム型**すべてが `src/structure_pipeline.rs` に dispatch 先と scripted oracle TSV を持つ。残る作業は：
  - [x] **P2-1-T0 (`2ee2560`)**: `StructurePiece.placeBlock` の `SHAPE_CHECK_BLOCKS` マーク（template/jigsaw 経路）＋ `Feature.markAboveForPostProcessing` ＋ promotion 限定 `BushBlock` 除去（`village-taiga` 0 / 884,736 達成）。
  - [x] **P2-1-T1**: チャンク昇格時（`postProcessGeneration`）の非空流体セルに対する fluid tick 予約・配送接続（`src/main.rs` の `structure_template::register_fluid_ticker` / `promotion_fluid_tick` 登録済み）。
  - [x] **P2-1-T2 (`2481b48`)**: `structure_pipeline::load_defs_inner` で1つの不正な構造物 JSON が全パイプラインを `None` に落とす欠陥を要素単位の fail-skip + ログに修正。
  - [ ] **P2-1-T3 & 残存構造物受入（Phase C-2 Batch B3 完了 `7f0f0e5`）**: `bb6ec7c`（`ruined_portal` B-run・`monument`）、`2481b48`（`igloo`）、`2a4eaf4`（`village_plains`）に加え、`7f0f0e5`（[evidence/gameplay/feature-parity/structure-batch-b3-20261120.md](../evidence/gameplay/feature-parity/structure-batch-b3-20261120.md)）で `desert_pyramid`・`shipwreck`・`ocean_ruin_cold`（tall seagrass 更新修正）を 0-diff 昇格（計 **19 チャンク 0-diff**）。残る HOLD（`buried_treasure`, `swamp_hut`：Audit 1 で特定された `valid_biome` quart 座標 noise biome 判定への修正対象）と Batch B4 を段階的受入。
- [ ] **P2-2 残るconfigured featuresと実chunk差分** — `bonus_chest` および End の `minecraft:end_spike` は実装・native oracle 照合・4 mutation ゲート済み（`src/feature.rs`, `src/generation_contract_tests.rs`, `tools/mutation_check.sh`）。残る `sculk_patch` 等の最新HEAD・同一population順再計測と `sculk_gate.py --require-match` 判断を閉じる。
- [x] **P2-3 三dimensionの通常サーバー経路（Stage F2 の中核）** — `MC_SPINE_WG_PRESET=end|nether` は offline 診断限定のため、以下の S0〜S4 で通常サーバーの3次元生成・保存・配信・移動を完成済み：
  - [x] **P2-3-S0 (`ffb7a22`)**: `tests/protocol767.py` CLI ハーネスの次元対応（`dimension_type` の `min_y`/`height`/`has_skylight` 動的解釈）。
  - [x] **P2-3-S1 (`71e96a5`)**: `DimId` (`Overworld | Nether | End`) と次元別 `Gen` レジストリ（同一 seed 初期化）、`chunk_data` / `write_light_arrays` への `DimId` 配線。
  - [x] **P2-3-S2a (`2e95617`)**: `WORLD` オーバーレイの `DimChunkKey = (DimId, i32, i32)` 化・次元別コンパニオンセーブ・旧 `world_overlay.bin` 後方互換（選択 mutation 4/4 proved）。
  - [x] **P2-3-S2b〜d (`1ec70b0`, `0e532bb`)**: 残り 8 ストア（`S2b`: `CHESTS`/`FURNACES`/`CROPS`、`S2c`: `SIGNS`/`PENDING_BLOCKS`/`BACKOFF`/`PLUGIN_CELLS`、`S2d`: `POS_STORE`）の `DimId` キー化。
  - [x] **P2-3-S3 (`3a3bb37`)**: Nether/End の 16 セクション（`0..256`）チャンク構築と `has_skylight == false` 時の `skyLightMask = 0` 配信。
  - [x] **P2-3-S4 (`c40417d`)**: `0x2b` Join Game / `0x47` Respawn による次元間遷移・ポータル移動・ログイン次元復元の wire 検証（`tests/dimension_wire.py` PASS、選択 mutation 8/8 proved）。

### P3 — M2 gameplay / M3性能（WorldGenと証拠を混ぜない）

- [x] **P3-1 M2観測可能なゲーム操作** — [合格範囲](PLAN.md)・[Horizon計画](horizon-gameplay/README.md)。Slice 3a〜3d（チェスト/樽/エンダーチェスト・遅延 LootTable・waterlogged chest、`0fa6807`）、Slice 4a〜4c（衝突・流体高/水流押し・押し合い/空間グリッド）完了。
  - [x] **Slice 4d (`b822d99`)**: `LivingEntity.travel` の水中/溶岩移動・浮力・落下減衰と Nether `ultraWarm` 溶岩押し流し倍率（`0.0023333333333333335`）。
  - [x] **一般 1.21.1 `BaseSpawner` + `SpawnPotentials` (`b2e41b5`)**: `src/spawner.rs` の重み付きポテンシャル・カスタムスポーン規則・NBT 往復。
  - [x] **Phase C-1 M2 実 wire 往復検査 (`6b07fee`)**: 戦闘・死亡/リスポーン・2×2 クラフト/かまど精錬・天候/昼夜永続化の CLI 実 wire 往復検査（`tests/m2_gameplay_wire.py` PASS、`tests/verify.sh` 統合済み）。
  - [x] **Phase D-1 ランタイム Lock 順序・EID 衝突・次元別流体/作物修正 (`d0f0afd`)**: `FURNACES`/`CROPS`/`Mob.pos` のロック順序逆転解消、`load_world` 保存ドロップと Mob EID の衝突防止、Nether/End の次元別プレイヤー流体（溺水/溶岩）＆作物成長修正（`d1_` 3 tests + `m2_gameplay_wire.py` & `dimension_wire.py` PASS）。
- [ ] **P3-2 生成の並列化と性能gate（M3 P1〜P5 + 超高効率計算 Layer A1〜A4）** — [仕様](chunk-parallelism/README.md)。実装完了（代表負荷 `WARM=1` 対照の最終判定は `P4-1` で統合確認）：
  - [x] **M3 P1a (`3135354`) / P1b (`b588c03`)**: 負荷非依存カウンタ（M1〜M5）＋ラウンド単位 `TerrainShare`・single-flight `starts`/`rings`・`Arc` `terr_c` により、`full` CPU +88% ペナルティを完全解消。
  - [x] **M3 P2a (`43a463b`) / P2b (`0469ad9`) / P2c (`c06be7c`)**: 常駐 `PalettedSection` モジュール（`src/paletted.rs`）、`SECTION_STORE`・zero-repack チャンク配信、`PalettedSection::uniform_state()` O(1) uniform fast-path。
  - [x] **M3 P3a (`1befd35`) / Layer A3-a/b/c (`2a4eaf4`, `b105d58`, `91b19c4`) / A3-a キャッシュ共有 (`d918eac`)**: Carver dense map + `OriginGrid` + `carve_batch`、Feature `u128` バイオームマスク、バッチ pre-carve + 依存 DAG 並列 `populate_parallel` ＋ ワーカー `col_c`/`chunk_biomes` 共有（全次元 SHA-256 ビット完全一致）。
  - [x] **M3 P4a (`06abd5f`) / Layer A1-a/b/c (`3cb6d24`, `b981704`, `d918eac`) / Layer A2-a/b/c (`c745772`, `785cdf5`, `4d28f01`)**: Aquifer `AqChunkGrid` 化、`beard`/stronghold 遅延化、2D コーナーメモ＋AVX2/AVX-512 `DfProg`/noise、lazy surface mats、`Cond::Biome(u128)`＋`Rule::Seq` 直接反復。
  - [x] **M3 P5 & Layer A4 (`ea59c4e`, `b2e41b5`, `c06be7c`, `e82fbc6`)**: 配信パレット書込 -89%、sky spill -52〜63%、floor cache -88%、`EntitySpatialGrid` 単一マップ化、512-entry `MapChunkCache`、`tools/pgo_build.sh`（PGO `full j=8` `2.971s -> 2.656s`、-10.6%、全 dump SHA-256 ビット完全一致）。

### R5 — Mod 互換層（NeoForge 1.21.1 S0〜S3+）

- [x] **NF-Arch（設計確定 `da9ae45`）**: [neoforge-architecture.md](mod-compatibility/neoforge-architecture.md)（5 レイヤ構成・`DimId` 対応・バッチ JNI・`shimcheck` 起動前拒否）。
- [x] **NF-0 (S0 `shimcheck` `a092e5d`)**: `ShimCheck.java`（ASM `9.10.1` + `neoforge.mods.toml` 解析）＋ `tools/shimcheck.py` ＋ `tests/shimcheck_test.py`（33 tests PASS）。
- [x] **NF-1 (動的 Bootstrap & Dual Event Bus `23edfdc`)**: `ShimCheck` ゲート付き複数 Mod 動的ディスカバリ・コンストラクタ注入・`Bus.GAME` / `Bus.MOD` 振り分け・`Dist.CLIENT` 除外・`tests/neoforge_bootstrap_test.py`（計 45 tests PASS）。
- [ ] **NF-2〜NF-4 (S1〜S3)**: S1 主要 6 イベント + `MutationBuffer` → S2 `DeferredRegister` + 名前ベース永続化 → S3 Direct `ByteBuffer` バッチ JNI。

### R6 — クライアントフォーク計画と Server-Client 共通カーネル（`mc-spine-client`）

- [ ] **CF-0（Server-Client 共有コアの切り出し）**: `src/block_registry.rs` (`26,684` states / `FULL`/`CENTER`/`RIGID`)、`src/entity_collision.rs`、`src/entity_fluid.rs`、`src/paletted.rs`、`src/lighting.rs`、`src/nbt.rs`、`src/brigadier.rs` をサーバーとクライアントフォークの共有クレート境界（Zero-Drift Shared Kernel）として切り出す。
- [ ] **CF-1（ヘッドレス 1.21.1 決定論的シミュレータ & E2E ボットクライアント）**: 受信チャンク (`0x27`)・ブロック更新 (`0x09`)・エンティティ移動 (`0x5a`) を共有カーネル上で毎 tick ローカル予測し、Vanilla 1.21.1 サーバーおよび `mc-spine` サーバーとの位置・流体・衝突誤差 0 を検証する。
- [ ] **CF-2（クライアント描画・増分ライティング・Greedy Meshing 接続）**: AGPL コード（Torch 等）を流用せず、`PalettedSection` と `lighting.rs` から直接セクションメッシュを生成する描画パイプラインを接続（Issue `#43` と連携）。
- [ ] **CF-3（`mods/` 投入型 Rust Integrated Server — `mc-spine-integrated.jar` / `cdylib`）**: 既存 Fabric/NeoForge クライアント（Sodium/Iris 等）の `mods/` に 1 個 JAR を入れるだけで、シングルプレイ／LAN ホストのサーバー処理を 100% Rust (`libmc_spine` + 共有メモリ / Direct `ByteBuffer` FFI) にオフロード。
- [ ] **CF-4（Default Launcher / Prism Launcher ネイティブ起動パッケージ）**: Custom Version JSON + 極薄 Bootstrapper JAR（約 20 KB、起動 50ms 後に JVM 退場）および Prism `Wrapper command`（JVM 非起動で直接 Rust 起動）により、公式/Prism/Modrinth 全ランチャーで内部 100% Rust ネイティブ動作を実現。

### P4 — 統合前のゲート（自動ではmergeしない）

- [ ] **P4-1 同一SHAの全gateと独立レビュー** — `tests/verify.sh` 完走、native oracle、本番wire、mutation、serial/parallel、adapter、layers/debt、Fossil/structure/feature・dimension回帰の結果を同一revisionで収集。hosted CIを使えない場合は未実行と明記。過去の[Issue #7](https://github.com/nekomario28/mc-spine/issues/7)はクローズ済み。
- [x] **P4-2 配布物・ライセンス・リリース判定（Stage F1 完了 `88573be`）** — P0-3（D1〜D5）および `GPL-3.0-or-later`（寄稿は `LGPL-3.0-or-later` とのデュアルライセンス）確定、`main` への Fast-Forward 統合完了。

## 次の着手

[ROADMAPの直近の実行順](ROADMAP.md#直近の実行順2026-11-13)を参照する。
最初は既存captureの再現診断とsnowyセルの読み取り位置・writer比較。
P1-1/P1-2/P1-3は既存の限定実装を再利用して不足する出口だけを追加する。
下記の作成時点の「未実行」を、現在の未実装一覧や最新gate結果に読み替えない。
資産・出所の監査は未完であり、新しいデータの再配布やリリースの承認にはならない。

## 履歴: 作成時点の作業実績（2026-11-06〜07）

以下は当時の記録。現在の状態は上の棚卸しと各sliceを優先する。

- **P0-2 は部分対応・未完了**: [Draft PR #32](https://github.com/nekomario28/mc-spine/pull/32) で、未知のdensity型をゼロ代替しない修正と、`noise`/ `shifted_noise` の不正入力拒否・Y軸シフトを追加。固定データのdensity定義35件とnoise settings 7件についてJSON型名を静的に確認し、parserの型分岐に未対応の名前は見つからなかった。**Rustビルド・全体suite・ネストした値の実行・native比較は未実行**。明示ゼロ扱いの意味論、loaderの部分失敗とsurface/biomeの検証は残す。
- **P0-3 は追跡実態を確定・未完了**: [data tracking監査](../evidence/foundation/data-tracking-20261106/README.md) にGitツリーの数量・ファイル・文書矛盾を記録。出所・バイト照合・消費者切替・歴史的ライセンス問題の解決には未着手。データ削除やリリースはしていない。

- **P1-1 は部分実装・未完了**: [Draft PR #33](https://github.com/nekomario28/mc-spine/pull/33) で、既存Treeと共通の葉tick予約、フルキューブに接するフェンス・ガラス板/鉄格子・限定条件の壁、既存階段の内角/外角計算、waterlogged属性保持、runtimeの松明/ランタン支持喪失とexactly-once dropを追加。ポータブルPython 6件とスタブ環境のJava構文コンパイルのみ成功。**Rust compile/実行・公式jarのnative照合・tick実物理・汎用neighbor chain・水没fluid配送は未完了**。既存gravity/leaf/fluid runtimeを重複実装せず利用し、P1-1チェックは未チェックのまま維持。参照：[段階的実装の境界](https://github.com/nekomario28/mc-spine/blob/feat/template-neighbor-finalization-20261106/specs/template-finalization/slices/01-neighbors.md)。

- **P1-1 続報（2026-11-06）**: Draft PR #33でフェンス/ガラス板・鉄格子の接続と切断、壁の直線・T字（覆い無しに限定）、既存runtimeの連鎖更新、水没ブロックのwater source判定と+5ゲームtickのfluid scheduling、生成時proto tick予約まで追加。Java nativeオラクル25ケース、Rust回帰追加。**native JAR実行・Rust compile/実行・網羅的な支持判定・複雑な壁・全配送比較は未完了**のため、P1-1は未チェック。

- **P1-1 Native検証基盤の追加（2026-11-07）**: [Draft PR #34](https://github.com/nekomario28/mc-spine/pull/34) を #33 の上に積み、固定1.21.1の全1,060ブロック種類・26,684状態×12支持場面＝320,208件を `BlockState.canSurvive` で判定するオラクルを実装。Nativeタグ読込、形状更新時のset/tick要求順、Native `LevelTicks` へ投入した後の配送順、ポータブルな欠落/順序破壊テストを追加。**全状態Native実行・Rust本体との一致、ServerLevelの連鎖・tick効果、全環境における支持判定は未完了**。本項を完了とは扱わない。

## 履歴: 作成時点のレビュー注意

作成時点のHEADコミット本文にはRustテスト **384 passed / 1 ignored**、
**255 mutants** が記録されている。これは当該コミットの自己申告の検証記録で、
本計画PRで独立再実行した結果ではない。hosted CIにも当該作業ブランチの実行記録はない。
当時の残件には汎用neighbor遷移、template entityの実初期化、
unrestricted runtime tick、全chunk parityの実装/受け入れが残る。
