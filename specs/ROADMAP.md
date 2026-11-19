# ロードマップ

承認済みの全体方針A〜E、選択mutationによる統合と定期全件監査、資源予算は
[プロジェクト収束計画](project-convergence/README.md)を参照してください。
chunk状態の整合性・操作から保存再開までの完成・統合を主線とし、計画と設計はmain agentが担当します。
個別候補の検証完了・取り込みと、未決の設計判断は別途確認します。

現在の状態は[README](README.md)、目標と合格条件は[PLAN](PLAN.md)、測定値と実行条件は各 `evidence/` を正本とします。この文書は**次に進む順序・依存関係・出口条件**を定めます。部品の実装、実クラスとの部分一致、全体合格を混同しません。

現行の `perf/worldgen-parallel` で進める **ID付きの実装順・未完了チェック・検証ゲート**は[実装・検証チェックリスト](implementation-checklist.md)に一覧化する。チェックリストは実行台帳であり、この文書・各spec・`evidence/` の正本を置き換えない。

## いまの判断 — 限定fixtureのruntime一致から全worldgenへ

**M2未達**は変わりません。forest `(-64,32)` の3×3・seed 0・structures=falseで、明示した25 source chunk順に通常バイナリを実行し、paused nativeの884,736セルすべてが0差になりました。leaf distanceも流体も除外していません。

`5b0e4a2` のcheckpointでは、portの通常game_tickで生成時との差1,803セルも再現し、700 tick後に全セル0差となりました。5 tickで保存し別プロセスで695 tick実行する経路も0差です。正本は[feature contracts](../evidence/gameplay/forest-r1/feature-contracts.md)です。現在の拡張変更は別途回帰を要求します。

制約:
- source footprintだけでなく**順序も入力**。既定順序は「規律の名前つきメンバー」（level優先キュー + 宣言済みtie-break、`population_discipline`）であり、vanilla ticket schedulerの再現でもcaptureの再現でもない。captureと比較する行は順序を明示入力する。
- 属性による除外をしない。stageは実際の時計・writer・保存状態で確定する。
- `main` ブランチは `perf/worldgen-parallel` と統合・同期済み。hosted CIは課金設定により未起動の場合があるため、ローカル実行結果とhosted CIを混同しない。
- 全seed/biome/構造物の合格ではない。JVM identity-hashによるleaf tree-bin tieも明示した未対応境界に残る。

**現在の作業:** [全worldgen](worldgen-parity/README.md)の残存構造物受入（Batch B4 / HOLD 解消）、Audit 1 ホットパス修正（Phase D-2）、NeoForge S1〜S3 拡張（R5 `NF-2`〜`NF-4`）、およびクライアントフォーク／ランチャー統合基盤（R6 `CF-0`〜`CF-4`）。

## 直近の実行順（2026-11-20 同期）

長期の R0〜R6 は維持し、現在の並列トラック（[CONTRIBUTING.md](../CONTRIBUTING.md#並列開発のモジュール分担コンフリクト防止ルール) の Track 1〜3）に沿って次の小さい出口で進める。[実行台帳](implementation-checklist.md)で実装済み・限定検証済み・未検証を分け、[調査手順](worldgen-parity/investigation-workflow.md)を各差分の着手前に確認する。

| 優先トラック | 対象 | 次の出口 |
|---|---|---|
| **Track 1 (Worldgen & Perf: `Phase D-2` / `P2-1-T3`)** | Audit 1 ホットパス修正 ＆ 残存構造物 Batch B4 | ① `Rule::Seq` ゼロアロケーション化・`gen_basalt_columns` の `ctx.g.sea` 参照・`OriginGrid` 疎バッチ保護・Woodland Mansion 基礎の `world.min_y()` / `is_liquid_block` 修正、② `valid_biome` の quart 座標 noise biome 判定修正による HOLD 2件（`buried_treasure`, `swamp_hut`）の 0-diff 昇格と残存構造物バッチ受入 |
| **Track 2 (Mod 互換: `R5 NF-2〜NF-4`)** | NeoForge 1.21.1 S1〜S3 実装 | `NF-0` (`a42ebb2`) / `NF-1` (`6d3a03d`) の動的ブートストラップ＆Dual Event Bus 上に、S1 主要 6 イベント + `MutationBuffer` (`NF-2`) → S2 `DeferredRegister` + 名前ベース永続化 (`NF-3`) → S3 Direct `ByteBuffer` バッチ JNI (`NF-4`) を接続 |
| **Track 3 (Client Fork & Runtime: `R6 CF-0〜CF-4` / `P1`)** | 共通カーネル・ランチャー統合 ＆ Template 最終化 | ① `tools/layers_check.sh` の 44 モジュールグラフを維持したまま共通カーネル（`CF-0`）とヘッドレス予測検証（`CF-1`）・`mods/` 投入型 Integrated Server（`CF-3`）・極薄 Bootstrapper JAR / Prism Wrapper（`CF-4`）を整備、② `P1-1〜P1-4` の実チャンク Fossil / 汎用 neighbor 最終化 |
| **Release Gate (`P4-1`)** | 同一 SHA での代表負荷 back-to-back 再測定と全体検証 | `tests/verify.sh` 完走、`tools/perf_bench.py` / `WARM=1` 代表負荷での M3 再測定、選択 mutation、layers/debt/provenance ゲートの同一 SHA 収集 |

### 公開可能フォーク（Public Fork Release）への収束実績と Phase D 計画（2026-11-20）

#### Stage F1: 法的にクリーンな Experimental / Alpha 公開フォーク（P0-3 / D1〜D5 完結）
- **F1-1 (`P0-3 D1/D2`: 公式 `server.jar` からの全資産ローカル生成化 — 完了 `4b43735`)**:
  - `tools/vanilla_data/gen_vanilla_data.sh`（＋`verify_pack.py`、`--verify` 2,350 ファイル照合）と `assets/block_states_1211.json` の jar 自前生成化を完了。
- **F1-2 (`P0-3 D3/D4`: GPL 同値アセット・出所不明ファイルの完全抹消と fail-closed ゲート — 完了 `4b43735`)**:
  - 旧 `assets/pblocks.json`・`assets/properties.json` と `resourcepack.zip` を消費者ごと切替・除去し、`git filter-repo --invert-paths` で過去全履歴からも完全抹消済み。`tools/provenance_check.py`（`--self-test` 11/11 PASS、`--strict-public` モード付き）で再混入を常時ブロック。
- **F1-3 (`P0-3 D5`: トリプルライセンス寄稿条項とクリーン公開エクスポート — 完了 `04f61a7`, `bd06c8a`)**:
  - 本体配布ライセンスを **`GPL-3.0-or-later`**（[LICENSE](../LICENSE)）、寄稿およびファーストパーティコードを **`LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` のトリプルライセンス**（[COPYING.LESSER](../COPYING.LESSER) / [LICENSE](../LICENSE) / [COPYING.AGPL](../COPYING.AGPL)、[LICENSING.md](../LICENSING.md)）に統一。また、Mojang 由来生成物を一切含まない単一ルートコミット公開ツリーを出力する `tools/export_public_branch.sh`（`tests/public_export_test.py`）も完備。

#### Stage F2: 3次元・構造物・M2/M3 軽量性を備えた実用公開フォーク
- **F2-1 (`P2-3-S0〜S4`: 3次元サーバーの通常経路とワールドストア次元分離 — 完了 `c543c92`)**:
  - `S0`（済 `b38aa75`）・`S1`（済 `4576ec1`：`DimId` と次元別 `Gen` レジストリ）・`S2a`（済 `526c338`：`WORLD` オーバーレイ `DimChunkKey` 化と次元別セーブ互換）。
  - `S2b〜S2d`（済 `77db4a6`, `0c0ecbb`）：残り 8 ストア（`CHESTS`/`FURNACES`/`CROPS`/`SIGNS`/`PENDING_BLOCKS`/`BACKOFF`/`PLUGIN_CELLS`/`POS_STORE`）の `DimId` 化。
  - `S3`（済 `e2c8939`）：Nether/End の 16 セクション（`min_y = 0`, `height = 256`）チャンク配信と `has_skylight = false` での `skyLightMask == 0` 送信。
  - `S4`（済 `c543c92`）：`0x2b` Join Game / `0x47` Respawn による次元間遷移・ポータル移動・ログイン時次元復元（`tests/dimension_wire.py` PASS、8/8 mutations proved）。
- **F2-2 (`M3 P1〜P5` + 超高効率計算 Layer A1〜A4 — 実装完了 `d06032e`)**:
  - `P1a`（済 `7c00617`）・`P1b`（済 `5f01327`）・`P2a`（済 `src/paletted.rs`）・`P2b`（済 `0ef6a60`：`SECTION_STORE` + zero-repack wire）・`P3a`（済 `61d516d`）・`P4`（済 `3e1ba0b`）・`P5-wire/light`（済 `d4fa504`）に加え、**Layer A1-a/b**（済 `101b9f7`, `cd3abf9`）、**Layer A2-a/b/c**（済 `79a2002`, `2733c13`, `7b3917f`）、**Layer A3-a/b/c**（済 `b1beec5`, `c0478a1`, `c99fff9`）を統合。
  - **Phase A-2 / B-1 / B-2 / B-3 完了**: `482e81b`（`populate_parallel` ワーカー `col_c`/`chunk_biomes` 共有＋`Cond::Biome(u128)`＋`Rule::Seq` 直接反復）、`0c88980`（`EntitySpatialGrid` 単一マップ化）、`30d0346`（`PalettedSection::uniform_state()` O(1) fast-path＋512-entry `MapChunkCache`＋決定論的 `hidden_filler_with`）、`8d047a9`（`P0-2` 到達性監査スナップショット＆`u128` アンカー）、`d06032e`（`tools/pgo_build.sh` ＋ PGO A/B 実測 `full j=8` 2.971s → 2.656s、-10.6%、全 dump SHA-256 ビット完全一致）。
- **F2-3 (`P2-1 / P2-2`: 構造物実チャンク受入の拡大と残存 Feature / 境界の閉鎖)**:
  - `P2-1-T0`（済 `35cf12c`）・`P2-1-T1`（済 `promotion_fluid_tick` 登録済み）・`P2-1-T2`（済 `5fb816b`：要素単位 fail-skip）・`P2-1-T3` Batch B1〜B3（済 `5fb816b`, `b1beec5`, `ba6e2c3`：`ruined_portal` / `monument` / `igloo` / `village_plains` / `desert_pyramid` / `shipwreck` / `ocean_ruin_cold` 等 **計 19 チャンク 0-diff 昇格済み**）・`P2-2` `minecraft:end_spike`（済・oracle 検証＆4 mutation ゲート済み）。
  - 残件：HOLD 2件（`buried_treasure`, `swamp_hut`：`valid_biome` の quart 座標判定修正で解消見込み）と Batch B4 の段階的受入。
- **F2-4 (`M2 Gameplay`: 流体 `travel`・一般 Spawner・実 wire 往復・ランタイム Lock 安全性 — 完了 `ff8addf`)**:
  - `Slice 4d`（済 `e15a7ce`：`LivingEntity.travel` 流体浮力・減衰・Nether `ultraWarm` 溶岩押し流し）、一般 1.21.1 `BaseSpawner` + `SpawnPotentials`（済 `0c88980`）、戦闘/死亡/リスポーン・クラフト/精錬・天候/昼夜の CLI live wire 往復検査（済 `2a68722`, `tests/m2_gameplay_wire.py`）、流体イベント厳密トレース比較器（済 `9e5a283`..`e79cb4d`, `tools/fluid_trace_diff.py`）、および Furnace/Crop/Mob ロック順序逆転解消・EID 衝突防止・次元別流体判定（済 `ff8addf`）。
- **R5 並行トラック（NeoForge 1.21.1 互換層 `NF-0〜NF-4`）**:
  - 設計書 [neoforge-architecture.md](mod-compatibility/neoforge-architecture.md) 確定（`3efb811`）。`NF-0`（S0 `shimcheck` `a42ebb2`：33 tests PASS）および `NF-1`（動的 Bootstrap & Dual Event Bus `6d3a03d`：45 tests PASS）まで完了。次は `NF-2`（S1 主要 6 イベント + `MutationBuffer`）。

#### 超高効率計算＋収束加速の統合実行計画（Phase A〜C & D-1 完了 ＋ Phase D-2 / NF-2 / R6 実行計画、2026-11-20 同期）

完成速度と並列スループットを最大化するため、**5 つの収束規律（B1〜B5）** を固定する：
- **B1**: Stage F1/F2 のクリーンなプロベナンスとトリプルライセンス寄稿境界を維持し、いつでも外部公開・再ライセンス可能な状態を保つ。
- **B2 (`src/main.rs` 単一レーン規約)**: 肥大化した `src/main.rs` の衝突を避けるため、ロジックは既存の専用モジュールへ厚く閉じ、**1 バッチ内で `src/main.rs` を編集するレーンは常に 1 つ**に固定して並列レーンをコンフリクトゼロで回す。
- **B3 (構造物バッチ受入工場)**: `tools/structure_capture.py` をバッチ化し、未受入定義を「0-diff 自動昇格」「共通カーネル原因クラスタ」「個別原因」に自動仕分けして波及の大きい 1 修正で複数構造物を同時 0-diff 化する。
- **B4 (2 階層高速ゲート＋自動ベンチ)**: サブエージェントは対象フィルタ＋`layers_check`/`debt_ratchet`/`provenance_check`、インテグレータは `TMPDIR=target/tmp` 分割実行＋`tools/perf_bench.py`（SHA-256・M1–M6・wall/CPU 中央値自動計測）で 600s タイムアウトと手動計測ロスを根絶する。
- **B5 (台帳同期)**: フェーズ完了ごとに `specs/` と運用文書を実態へ同期し、陳腐化による再調査ロスをゼロにする。

| フェーズ | Lane 1（`src/main.rs` 専任） | Lane 2（計算・構造物専任） | Lane 3（法務・互換・独立モジュール専任） | 出口マイルストーン |
|---|---|---|---|---|
| **完了済 (Wave 0〜3: `526c338`〜`c99fff9`)** | `P2-3-S2b〜d` + `S3` (`77db4a6`, `0c0ecbb`, `e2c8939`), `M3 P2b` (`0ef6a60`) | `perf_bench.py` (`7c52be5`), Layer A1-a/b (`101b9f7`, `cd3abf9`), Layer A2-a/b/c (`79a2002`, `2733c13`, `7b3917f`), Layer A3-a/b/c (`b1beec5`, `c0478a1`, `c99fff9`) | `igloo` + `P2-1-T2` (`5fb816b`), `village_plains` 0-diff 確認 (`b1beec5`) | 3 次元配信・パレット化・AVX-512・DAG 並列基盤完了 |
| **完了済 (Phase A: `04f61a7`〜`482e81b`)** | **A-1**: `P2-3-S4` (`c543c92`, `tests/dimension_wire.py` E2E) | **A-2**: `Layer A3-a` ワーカーキャッシュ共有 (`482e81b`) | **A-3**: Stage F1 `D5` (`04f61a7`, `export_public_branch.sh`) | Stage F1 完結、3 次元移動 E2E 完成、DAG 並列の実測高速化 |
| **完了済 (Phase B: `0c88980`〜`d06032e`)** | **B-1**: `Layer A4` (`30d0346`: `uniform_state()` O(1) fast-path + 512-entry `MapChunkCache`) | **B-2**: `Layer A1-c` (`482e81b`: `Cond::Biome(u128)` + `Rule::Seq`) ＋ `P0-2` 監査 (`8d047a9`) | **B-3**: `BaseSpawner` + `EntitySpatialGrid` (`0c88980`), PGO & 計測記録 (`d06032e`) | M3 / Layer A1〜A4 完了（PGO `full j=8` -10.6%、全 dump SHA-256 一致） |
| **完了済 (Phase C + NF-0/NF-1: `8f62f9f`〜`bd06c8a`)** | **C-1**: M2 残 wire 往復検査 (`2a68722`, `tests/m2_gameplay_wire.py`) | **C-2**: `P2-1-T3` Batch B3 (`ba6e2c3`: `desert_pyramid`, `shipwreck`, `ocean_ruin_cold` 0-diff 昇格、計 19 チャンク 0-diff) | **NF-0 & NF-1**: S0 `shimcheck` (`a42ebb2`) ＋ 動的 Bootstrap & Dual Event Bus (`6d3a03d`, 45 tests PASS) ／ **C-3**: 全モジュール警告 68→0 (`8f62f9f`..`ee2ff90`) ＋ トリプルライセンス & 過去履歴不純ファイル完全抹消 (`bd06c8a`) | Stage F2 実用公開フォーク到達 ＋ NeoForge S0/NF-1 動的ゲート確立 ＋ `main` 統合 |
| **Phase D（Audit 1/2 修正 & Client Fork R6）** | **D-1**（済 `ff8addf`）: ランタイム Lock 逆転（Furnace/Crop/Mob）・`load_world` EID 衝突・次元別 `set_block_dim`/プレイヤー流体判定修正（3 tests + `m2_gameplay_wire.py` & `dimension_wire.py` PASS） | **D-2**: Audit 1 ホットパス（`Rule::Seq` ゼロアロケーション化・`ctx.g.sea`・`OriginGrid` 疎バッチ保護・Mansion/Structure `valid_biome` quart 判定） | **NF-2 / R6**: S1 6イベント & `MutationBuffer` ＋ クライアントフォーク（`mc-spine-client`）共通契約・ランチャー統合基盤（`CF-0`〜`CF-4`） | 全監査指摘の閉鎖 ＋ Server/Client 協調設計への拡張 |

**原因記述の訂正:** 端のair差とsnowy差を位置の近さだけで同じ原因にまとめた推測は
撤回する。訂正根拠・区別すべきlookup・次の反証は
[chunk差分記録](template-finalization/slices/04-chunks.md)を参照。
これはcarver全体の合格を意味しない。

## R0 — 自前データの意味論と回帰の基準を閉じる

1. 全体テストの一次失敗とmutex poisonの連鎖を分ける。一次失敗は必要なら新しいプロセスで再現し、入力・loader・評価器・古いcaptureのどこに属するかを特定する。poisonを握り潰して緑にしない。
2. 残るdensity function、real-noise地形へのsurface適用（steep/条件式/合成scanは[照合済み](../evidence/foundation/surface-context/README.md)）、`spawn_costs`の適用を、固定1.21.1の実際の意味で評価する。aquiferのpreliminary surface・流体状態は[実クラス対照](../evidence/foundation/aquifer/README.md)、biome `carvers` のAIR listとown-jar configured_feature pathは[carverの証拠](../evidence/foundation/biome-carvers/README.md)、`spawners`の読み込みと夜間hostile選択は[biome spawnerの証拠](../evidence/foundation/biome-spawners/README.md)で限定範囲を閉じた。全seed/chunk、deep-dark実chunk、NaturalSpawner、spawn_costsのenergy budgetは未検証である。各差分を独立した小さい実クラス対照と本番経路のゲートにする。
3. surfaceはsourceのdecodeや温度条件、合成列のscanだけで閉じず、実際のbiome選択・実noise列・surface contextを含む適用結果を検査する。未知ノード・解決できない参照・落とした入力名は明示し、成功したように読み替えない。異なるseedや生成器の寿命をまたぐnoise cacheと、循環参照のplaceholderも、比較を隠さない条件で検査する。

**出口:** 比較対象のrouter/biome/surface経路に未説明のゼロ代替・参照落ちが残らず、未解決・未対応の一覧と本番での到達性を説明できる。ローカルsuiteの緑は必要条件であり、whole-world・native対照・CIの代用にしない。意味を変えた経路には実クラス対照と故障を検出するテストがある。全体の赤は一次原因へ帰属させ、原因未確認のcapture更新で消さない。個別の修正ごとに広い回帰を確認し、全体合格は必須テストが通ってから判定する。

根拠: [自前データ監査](../evidence/foundation/audit-snapshots/README.md)、[sampler/native noiseの検証範囲](../evidence/foundation/weird-sampler/README.md)、[aquiferの検証範囲](../evidence/foundation/aquifer/README.md)、[carverの検証範囲](../evidence/foundation/biome-carvers/README.md)、[biome spawnerの検証範囲](../evidence/foundation/biome-spawners/README.md)、[surface contextの検証範囲](../evidence/foundation/surface-context/README.md)、[surface loaderの検証範囲](../evidence/foundation/surface-loader/README.md)。

## R1 — 同じ生成stageでworldgenを再受け入れする

### R1.0 fixture契約（先に固定する）

1. referenceとportで **seed、data pack、structures有無、forceload範囲、dump範囲、margin** を同じにする。
2. referenceの全chunkが `full` であること、対象featureが実行されたこと、skip/excludedを記録する。
3. paused生成状態とtick後状態を別artifactとして保存し、時計とwriterを記録する。属性名からruntimeと判断しない。`--tick`の有無でセルを除外しない。
4. malformed/empty/truncated dump、wrong jar、stale provenanceは比較前に拒否する。

**出口:** 同じfixture定義を一つの文書に固定し、3x3 canonical referenceとportのtarget差分が再現する。marginを変えた比較は別fixtureとして記録し、前の数値と混ぜない。

### R1.1 tree / surface / carver（このcheckpointで閉じた範囲）

- targetのterrain/surface/carver: 0差
- tree origin 41/41の個別照合は維持。旧no-tick 0は属性除外を含む歴史値で、現在のfull-state受け入れの根拠にはしない。
- `min_clipped_height`なしのclip拒否、`dirt_provider`、fancy centre limb `h2 * 0.5`、fancy attachment開始位置を実クラスbytecodeと照合
- surface materialはactual pre-surface stateを分類し、`wtop - 4` windowだけに適用

**出口:** 現在のpaused full-state照合を維持し、理由のないcapture更新をしない。

### R1.2 feature生成境界（限定受け入れ）

coal/chest/grassだけでなく非targetを含む3×3全体が、paused nativeと全state一致した。対応するnative vectorsと本番dump経路は[feature contracts](../evidence/gameplay/forest-r1/feature-contracts.md)を参照する。

**出口:** 同じ明示source順で全884,736セル0差を維持する。全chunk FULL、全stateを比較し、部分比較・未実行・古いartifactを緑と扱わない。生成captureの更新はnativeの独立した証明の後だけ行う。

### R1.2b 保存後のruntime境界（限定fixtureのcheckpointあり）

- Native LeavesBlock/FlowingFluidの実bodyとpending tick ownershipを調べる。
- 既存runtime ownerへ統合し、第二の近似tick経路を増やさない。
- 最初に同じ生成状態とpending入力、その後に同じスケジューリング・時点を確認する。
- canonicalとの1,803セル差を数えたまま閉じる。writer帰属を実装済みと読み替えない。

**出口:** port自身のruntimeを通った保存状態が、同じ入力・時点のnativeと全セル一致する。全seed/構造物の受け入れとは分ける。

### R1.3 structures（R1.2の後）

- jigsaw、template pool、NBT template、processor、structure placementを独立ownerとして設計する。
- feature残差とstructure残差を同じ表で混ぜない。
- 対象構造物のreference chunkが完成し、正のセル数・block entity・template参照が比較できるまで「構造物対応」と主張しない。

**出口:** 対象構造物ごとに、同じseed/構造物設定で配置・セル・block entity・保存/再読込が一致する。未対応構造物は明示する。

## R2 — 部品の一致から、操作とlifecycleの一致へ広げる

R0/R1の比較基準を主線とし、独立したgameplay修正は小さいsliceとして進められます。ただし部分的な緑でworldgenの受け入れを代替しません。

- **inventory / chest:** slot挿入・転送から、実際の回収接触・delay/owner・actorとの状態共有、viewers・window/state ID・閉鎖/無効化・二重chestの配置・保存/再読込へ進む。特殊slotの制約は通常slotに混ぜない。
- **entity / collision:** packet角度と有限AABB clipの先に、実寸法・pose・最寄りhit・ブロック/voxelによる遮蔽・support/fluid・属性・追跡lifecycleを置く。現在の近似boxと終点だけのblock判定は完了条件にしない。
- **NBT / structures:** 必要な消費者を先に特定し、読み書き・名前/値・孤立surrogate・componentの長さ境界を往復検査する。modified UTF-8の書き出しやopaque lore保持だけで、任意NBTやtemplateを読めるとは扱わない。

**出口:** PLANのM2範囲を同じ入力のvanilla対照で操作し、状態更新・実通信・別接続からの再取得・保存/再読込を機能別に検査できる。helperだけの一致ではなく、本番の操作経路を通す。画面操作は要求せずCLIを使う。

既存の境界: [Horizonのgameplay計画](horizon-gameplay/README.md)、[Valence由来のbinary/geometry契約](valence-primitives/README.md)。donorの版・データは仕様の正本にせず、固定1.21.1の挙動を優先します。

## R3 — 同じ仕事での軽量性

M2で比較できる同じ機能・負荷・設定を固定し、back-to-back測定でCPU、メモリ、tick遅延を比較します。生成burstと定常区間を分け、最適化した経路の実行と出力不変を先に確認します。

旧ベースライン（[worldgen-cost](../evidence/gameplay/worldgen-cost/README.md)・[performance](../evidence/foundation/performance/README.md)）では、RSSはvanillaの約1/8（142対1,160 MiB）、定常tick本体は0.05〜0.17 ms/tick（20 TPS、予算超過0/100）である一方、移動時のCPUは1.6〜4.7倍重かった。これに対し、承認済みの **5段階超軽量化計画（P1〜P5）＋超高効率計算（Layer A1〜A4 + PGO）**（詳細は[チャンク生成の並列化](chunk-parallelism/README.md#m3-超軽量化-5段階計画p1p5承認済み)）をすべて実装・統合済み（`d06032e`）:

1. **P1 (`ChunkCols` / `BeardChunkData` 共有 — 実装済 `7c00617`, `5f01327`)**: ロード非依存カウンタ M1〜M5 ＋ ラウンド単位 `TerrainShare`・single-flight `starts`/`rings`・`Arc` `terr_c` により、`full − NOFEAT` の重複生成ペナルティを解消。
2. **P2 (常駐 `PalettedSection` ストア — 実装済 `0ef6a60`, `30d0346`)**: Single-value / Indirect / Direct の省メモリ section ストアを正本化し、zero-repack チャンク配信と `uniform_state()` O(1) スキップを統合。
3. **P3 (`CARVERS` / `FEATURES` 並列化 — 実装済 `61d516d`, `b1beec5`, `c0478a1`, `c99fff9`, `482e81b`)**: 共有 `OriginGrid`、`u128` バイオームマスク、バッチ pre-carve、依存 DAG 並列 `populate_parallel` ＋ ワーカー `col_c`/`chunk_biomes` 共有を統合（全次元 SHA-256 完全一致）。
4. **P4 (`DfProg` SIMD/バッチ評価と早期枝刈り — 実装済 `3e1ba0b`, `101b9f7`, `cd3abf9`, `79a2002`, `2733c13`, `7b3917f`)**: `AqChunkGrid`、2D コーナーメモ、AVX2 4-lane / AVX-512 8-lane `DfProg` & noise 評価、lazy surface mats、`Cond::Biome(u128)` を統合。
5. **P5 (ライト・M2物理/Mob・圧縮配信・PGO — 実装済 `d4fa504`, `0c88980`, `30d0346`, `d06032e`)**: 配信パレット書込 -89%、sky spill -52〜63%、`EntitySpatialGrid` 単一マップ化、512-entry `MapChunkCache`、`tools/pgo_build.sh`（PGO `full j=8` `2.971s -> 2.656s`、-10.6%）を統合。
6. **残る M3 出口作業**: Phase D-2（Audit 1 ホットパス修正：`Rule::Seq` ゼロアロケーション化・`OriginGrid` 疎バッチ保護）を反映したうえで、統合候補 SHA にて `WARM=1` 代表負荷の Vanilla vs `mc-spine` back-to-back 最終計測（`P4-1`）を実施する。

**ライセンス境界:** 本体は `GPL-3.0-or-later`（寄稿およびファーストパーティコードは `LGPL-3.0-or-later`、`GPL-3.0-or-later`、および `AGPL-3.0-or-later` のトリプルライセンス、[LICENSING.md](../LICENSING.md)）。**C2ME (`MIT`)**、**Lithium (`LGPL-3.0`)**、`Soju06/hyperchunk` (`MIT`)、`valence-rs` (`MIT`)、`MCHPRS` (`MIT`)、`Azalea` (`MIT`) は `LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` のすべてとライセンス互換である。外部からコード・手法を取り込む場合は帰属とライセンスを [THIRD_PARTY.md](../THIRD_PARTY.md) / [NOTICE](../NOTICE) に明記し、将来いつでも `LGPL-3.0-or-later`・`GPL-3.0-or-later`・`AGPL-3.0-or-later` の間を双方向に行き来できるよう、第三者の `GPL-3.0-only` / `AGPL-3.0-only` コードはファーストパーティファイルへ直接コピーせずアルゴリズム概念・ブラックボックス比較または専用モジュール境界で分離する。

**出口:** PLANのM3条件を代表負荷で満たし、再現手順・ノイズ・未対応機能を記録する。機能不足で仕事量が減った結果や、地形が異なる条件の旧測定を性能改善へ読み替えない。

## R4 — 対象revisionの統合・リリース判定

過去の統合線の検証を、今回の移行ブランチへ引き継ぎません。作業ブランチへのpushは保存・共有であり、mainへの統合や合格とは別です。

**出口:** PLANの合格条件と公開対象の機能範囲を満たしたうえで、統合候補そのものに対して、全体テスト、必要な並列回帰・wire・adapter・層/負債ゲート、変異テスト、独立レビューを完了し、ライセンスと既知の制約を確認する。通常のローカル統合では[承認された選択mutation方針](project-convergence/README.md#進行と検証の運用)を適用し、全campaignの監査結果と区別する。未実行・未開始のCIを緑とせず、ローカル実行とhosted CIを区別する。main更新やreleaseは、その判定とユーザーの指示を経て行う。

## R5 — 要求されたMod単位の互換拡張

順序と出口条件は[shim計画](mod-compatibility/shim-plan.md)（S0〜S5）に置く。最初は、mod jarが必要とする面を列挙してshimとの差分を出す `shimcheck`（S0、完了 `a42ebb2`）、次に動的ブートストラップと Dual Event Bus（NF-1、完了 `6d3a03d`）、NeoForgeの単純なMod（S1 / NF-2）、ブロック/アイテムを登録するMod（S2 / NF-3）、バッチ／スナップショット JNI 境界を持つ BlockEntity/tick 基盤（S3 / NF-4）の順に進める。shimの骨組みはユーザーのjarから手元で生成し、リポジトリには入れない。S3 基盤確立後は、別プロジェクトへ切り出さず `mc-spine` 本体上の「汎用オフロードカーネル群（接続グラフ・自動スリープ輸送/加工・Contraption 枠組み・リージョン負荷隔離）＋重量 Mod 固有の薄型加速アダプタ」へ段階拡張する。

**出口:** 対象Mod・版・API範囲・実行テストを特定し、その組み合わせに限った互換性を証明する。

## R6 — クライアントフォーク計画と Server-Client 共通カーネル（`mc-spine-client`）

サーバー（`mc-spine`）で確立した **Minecraft Java 1.21.1 (protocol 767) 厳密準拠のデータ駆動カーネル**をそのままクライアントフォーク（および公式 Launcher / Prism Launcher でワンクリック起動できる軽量 Rust クライアント／Integrated Server）へ展開するため、以下の 5 段階（`CF-0`〜`CF-4`）をロードマップに組み込む：

1. **CF-0（Server-Client 共有コアの公開 — `layers_check.sh` 互換の Zero-Drift Shared Kernel）**:
   - **モジュール層ゲート維持の設計原則**：`tools/layers_check.sh`（`src/main.rs` 起点の 44 modules / 111 edges 静的依存検査）を壊さないよう、いきなり別ディレクトリへファイルを移動して Cargo Workspace 分割するのではなく、`src/*.rs` のファイル配置と層ポリシー（`evidence/foundation/module-layers/policy.json`）を保ったままライブラリターゲット（`rlib` / `cdylib`）から以下の純粋カーネルモジュールを公開する：
     - **ブロック状態・ボクセル衝突・面支持**：`src/block_registry.rs` (`26,684` states、`FULL`/`CENTER`/`RIGID` 面支持)、`src/entity_collision.rs` (1.21.1 AABB 衝突解決・step-up)、`src/voxel_faces.rs`。
     - **流体物理・エンティティ移動**：`src/entity_fluid.rs` (`LivingEntity.travel` 水流・溶岩流・浮力・落下減衰)、`src/entity_push.rs`、`src/entity_spatial.rs`。
     - **チャンクストレージ・ライト・NBT・コマンド**：`src/paletted.rs` (`PalettedSection` wire zero-repack)、`src/lighting.rs` (`0x27`/`0x2a` ライトマスクと nibble 伝播)、`src/nbt.rs`、`src/brigadier.rs` (`0x11` コマンド木とクライアント側サジェスト評価)。
   - **メリット**：一般的なクライアント実装（Torch や Azalea 等）で最大のバグ源となる「サーバーとクライアントの物理・当たり判定・流体・ブロック予測の微妙なズレ（ラバーバンド現象・位置ズレ）」が、**同一の 1.21.1 Rust カーネルを共有することで構造的にゼロ**になる。
2. **CF-1（ヘッドレス 1.21.1 決定論的シミュレータ & E2E ボットクライアント）**:
   - 現行の `tests/protocol767.py` / `tests/m2_gameplay_wire.py` / `tests/dimension_wire.py` を発展させ、Rust 共有カーネル上で動く **ヘッドレス 1.21.1 クライアントランタイム**を構築する。
   - サーバーから受信した `0x27` Level Chunk With Light、`0x09` Block Update、`0x5a` Set Entity Motion、`0x47` Respawn をクライアント側 `PalettedSection` と `entity_collision` / `entity_fluid` で毎 tick ローカル予測実行し、Vanilla 1.21.1 サーバーおよび `mc-spine` サーバーの双方に対して「クライアント側予測位置 vs サーバー確定位置の誤差 0」を自動検証する。
3. **CF-2（クライアント側レンダリング・ライティング・メッシュ生成パイプラインとの接続）**:
   - Issue `#40`（解決済）・`#43`（Torch 増分ライティング比較）で整理した境界に従い、ファーストパーティファイルへ外部の `AGPL-3.0-only` コードを混ぜず（トリプルライセンス `LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` の双方向性を維持）、`mc-spine` の `PalettedSection`（`uniform_state()` O(1) スキップ・`hidden_filler`・`non_air` カウンタ）と `src/lighting.rs` から直接 Greedy Meshing / Chunk Section VBO を生成するクライアント描画層を接続する。
4. **CF-3（`mods/` 投入型 Rust Integrated Server — `mc-spine-integrated.jar` / `cdylib`）**:
   - 既存の Fabric / NeoForge クライアント環境（Sodium / Iris シェーダー等をそのまま利用）の `mods/` に JAR 1 個を入れるだけで、シングルプレイ／LAN ホスト時の重い Java Integrated Server を **JAR 内蔵の Rust ネイティブ `libmc_spine` (`cdylib`) + 共有メモリ / Direct `ByteBuffer` FFI** に完全置換する。
   - 描画・UI は使い慣れた既存クライアント Mod 資産を活かしつつ、ワールド生成（SIMD / Rayon DAG 並列）・構造物・照明・Redstone・流体・エンティティ Tick を 100% Rust で爆速実行する。
5. **CF-4（Default Launcher / Prism Launcher ネイティブ起動パッケージ — 極薄 Bootstrapper JAR & Wrapper Command）**:
   - **公式 Launcher / Prism / Modrinth App 全対応の Custom Version JSON + 極薄 Bootstrapper JAR（約 20 KB）**：ランチャーが起動する `mainClass` から `--username` / `--uuid` / `--accessToken` / `--assetIndex` / `--gameDir` を受け取って即座に Rust ネイティブバイナリへ引き渡し（`execve` 置換または JVM 即時終了）、**起動 50ms 以降は JVM メモリ 0・GC カクつき 0 の 100% 純粋な Rust ネイティブプロセス**として動作させる。
   - **Prism Launcher `Wrapper command` / `.mrpack` 直接起動**：Prism Launcher のラッパーコマンド機能に `mc-spine-wrapper` を指定することで、**一瞬たりとも `java` プロセスを起動せず**ランチャーのアカウント認証・アセット管理だけをそのまま流用して 0.1 秒で Rust クライアントを起動する。

**出口:** サーバーとクライアントフォークが 1.21.1 の `block_registry` / `paletted` / `entity_collision` / `entity_fluid` / `brigadier` を二重実装せず共有し、ヘッドレス予測検証（CF-1）、`mods/` 投入型 Integrated Server（CF-3）、および公式/Prism ランチャーからの 100% Rust ネイティブ起動（CF-4）が Vanilla 1.21.1 と完全互換で動作する。

## 横断規律

- [最小の因果差分から進める手順](worldgen-parity/investigation-workflow.md)を使う。既存captureの再利用→stage/writer特定→小さい実クラス対照→本番到達確認→compiling mutant→変更が固まった対象SHAの全gateと進める。原因の変わらない重い比較を繰り返さない。
- 比較前に、何が実行され、どのstageにあり、何がskipされたかを確かめる。値が変わらないだけで負の結果としない。
- source・固定具・実測binary・参照jarを結び付け、試験前後の正常系と変異後の復元を確認する。
- 古いcaptureの再記録を修正の代用にせず、意味の変化を説明してから更新する。
- 実装、部分parity、全体合格、性能改善、push、統合を別々に報告する。数値と履歴は証拠ページへ集約する。
