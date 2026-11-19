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
- mainの#22 data migrationとの統合は保留。hosted CIはbillingで起動しておらず、ローカル結果と混同しない。
- 全seed/biome/構造物の合格ではない。JVM identity-hashによるleaf tree-bin tieも明示した未対応境界に残る。

**現在の作業:** [全worldgen](worldgen-parity/README.md)。全64 biome、34構造物と3ディメンションへ広げ、配置候補・NBT/template・processor・jigsaw・構造物固有piece・地形/biome経路を実クラス対照で順に閉じる。部分的なhelper一致を構造物生成の受け入れにしない。

Mineshaftのadmission・dispatcher・piece配置は [Rust接続checkpoint](../evidence/gameplay/feature-parity/mineshaft-production-20261113.md) まで進んだ。freshな限定fixtureとの一致と元のA/B残差を分離している。[entity/NBT・保存再開・自然Mesa/複数startの追加検証](../evidence/gameplay/feature-parity/mineshaft-entities-20261113.md)も行ったが、車両物理・loot開封・spawner tickや全worldの一致は未完であり、P2-1やM2の完了にはしない。

## 直近の実行順（2026-11-13）

長期のR0〜R5は維持し、直近は次の小さい出口で進める。
[実行台帳](implementation-checklist.md)で実装済み・限定検証済み・未検証を分け、
[調査手順](worldgen-parity/investigation-workflow.md)を各差分の着手前に確認する。

| 順序 | 対象 | 次の出口 |
|---|---|---|
| 1 | P0-1 / P0-4: 基準と入力の固定 | seed 4242の両native runとportを、hash・絶対座標・stage・設定・skip一覧を持つ再現可能な診断にする。未確定入力を埋めるまで受け入れfixtureに昇格しない |
| 2 | P0-2 / P1-4: snowy残差 | 該当セルの上下state、quart/blended biome、実際の判定位置・温度・writerを比較。surfaceと雪配置を分けて最初の分岐を特定する |
| 3 | 残るair・ore型・planks差 | 各群で最初の相違stageとwriterを特定。形や近さだけでcarver/RNG/構造物に帰属させない |
| 4 | P1-1 → P1-2 → P1-3 / P1-4 | 汎用neighbor、実初期化、tick効果、正にFossilが置かれる実chunkを独立に閉じる。既存の限定契約は維持する |
| 5 | P2: 構造物と範囲拡張 | 必要なtemplate/processor/start/piece経路から、正の配置fixture、多seed・biome境界、通常Nether/Endへ段階拡張 |
| 6 | P3: M2 → M3 | CLI操作・保存再開を同一入力で受け入れ、その同じ機能/負荷でCPU・RSS・tick予算を測る |

資産の出所・再生成・配布方針（P0-3）は並行した残件。
内部調査の続行と、外部配布・リリースの承認は別判断である。

### 公開可能フォーク（Public Fork Release）への2段階収束計画（2026-11-19）

外部へ公開できるフォークを最短で作るため、**法的にクリーンな Alpha 公開（Stage F1）**と、**3次元・軽量性・主要ゲームプレイを揃えた実用公開（Stage F2）**の2段階に分け、現在解消すべき欠陥・不足を明示的なスライスとして実行順へ組み込む。

#### Stage F1: 法的にクリーンな Experimental / Alpha 公開フォーク（P0-3 / D1〜D5 完結）
- **F1-1 (`P0-3 D1/D2`: 公式 `server.jar` からの全資産ローカル生成化 — 完了 `762f312`)**:
  - `tools/vanilla_data/gen_vanilla_data.sh`（＋`verify_pack.py`、`--verify` 2,350 ファイル照合）と `assets/block_states_1211.json` の jar 自前生成化を完了。
- **F1-2 (`P0-3 D3/D4`: GPL 同値アセット・Mojang 派生物の追跡解除と fail-closed ゲート — 完了 `762f312`)**:
  - Pumpkin (GPL-3.0) 同値の旧 `assets/pblocks.json`・`assets/properties.json` と出所不明の `resourcepack.zip` を消費者ごと切替・除去し、`tools/provenance_check.py`（`--self-test` 11/11 PASS、`--strict-public` モード付き）をゲートに追加済み。
- **F1-3 (`P0-3 D5 / P4-2`: クリーン履歴ブランチ・ライセンス表記の確定 — 残件 1 スライス)**:
  - `--strict-public` が指摘する残 5 項目（`datapacks/vanilla`, `assets/block_states_1211.json`, `tests/data/biome_probe_1211.tsv`, `tests/data/vanilla_vectors_1211.tsv`, `config_packets.bin`）の配布扱い整理、`LICENSE`（Apache-2.0）・`NOTICE`・`THIRD_PARTY.md`・`README.md` の実ツリー完全一致、および過去履歴を含まないクリーン公開用ブランチ（orphan / squash 基点）手順の確定。

#### Stage F2: 3次元・構造物・M2/M3 軽量性を備えた実用公開フォーク
- **F2-1 (`P2-3-S1〜S4`: 3次元サーバーの通常経路とワールドストア次元分離)**:
  - `S0`（済 `ffb7a22`）・`S1`（済 `71e96a5`：`DimId` と次元別 `Gen` レジストリ）・`S2a`（済 `2e95617`：`WORLD` オーバーレイ `DimChunkKey` 化と次元別セーブ互換）。
  - `S2b〜S2d`（済 `1ec70b0`, `0e532bb`）：残り 8 ストア（`CHESTS`/`FURNACES`/`CROPS`/`SIGNS`/`PENDING_BLOCKS`/`BACKOFF`/`PLUGIN_CELLS`/`POS_STORE`）の `DimId` 化。
  - `S3`（済 `3a3bb37`）：Nether/End の 16 セクション（`min_y = 0`, `height = 256`）チャンク配信と `has_skylight = false` での `skyLightMask == 0` 送信。
  - `S4`（残・Phase A-1 回収中）：`0x2b` Join Game / `0x47` Respawn による次元間遷移・ポータル移動・ログイン時次元復元。
- **F2-2 (`M3 P1〜P5` + 超高効率計算 Layer A1〜A4)**:
  - `P1a`（済 `3135354`）・`P1b`（済 `b588c03`）・`P2a`（済 `43a463b`）・`P2b`（済 `0469ad9`：`SECTION_STORE` + zero-repack wire）・`P3a`（済 `1befd35`）・`P4`（済 `06abd5f`）・`P5-wire/light`（済 `ea59c4e`）に加え、**Layer A1-a/b**（済 `3cb6d24`, `b981704`）、**Layer A2-a/b/c**（済 `c745772`, `785cdf5`, `4d28f01`）、**Layer A3-a/b/c**（済 `2a4eaf4`, `b105d58`, `91b19c4`）まで統合完了。
  - 残る仕上げ（詳細：[chunk-parallelism/README.md](chunk-parallelism/README.md#超高効率計算アーキテクチャ計画layer-a1a42026-11-19-2e95617-基準)）：**Layer A3-a 速度回収**（`populate_parallel` の `TerrainShare`/owner キャッシュ共有）、**Layer A1-c**（`Cond::Biome(u128)` + `SurfaceProg`）、**Layer A4**（`PalettedSection` P2c–f O(1) light skip + 共有圧縮キャッシュ + `EntitySpatialGrid` ゼロアロケ/Mob SoA + PGO + `WARM=1` 対照ベンチマーク）。
- **F2-3 (`P2-1 / P2-2`: 構造物実チャンク受入の拡大と残存 Feature / 境界の閉鎖)**:
  - `P2-1-T0`（済 `2ee2560`）・`P2-1-T1`（済 `promotion_fluid_tick` 登録済み）・`P2-1-T2`（済 `2481b48`：要素単位 fail-skip）・`P2-1-T3` 第 1〜2 段（済 `bb6ec7c`, `2481b48`, `2a4eaf4`：`ruined_portal` B / `monument` / `igloo` / `village_plains` 0-diff 確認）・`P2-2` `minecraft:end_spike`（済・oracle 検証＆4 mutation ゲート済み）。
  - 残件：自動バッチ受入工場（B3/B4）による残存未検証構造物の段階的受入。
- **F2-4 (`M2 Gameplay`: 流体 `travel` と残る操作契約の閉鎖)**:
  - `Slice 4d`（済 `b822d99`：`LivingEntity.travel` 流体浮力・減衰・Nether `ultraWarm` 溶岩押し流し）。
  - 後続スライス：一般 `BaseSpawner`・構造物 Entity スポーン初期化と `spawn_overrides`、戦闘/死亡/リスポーン・精錬・天候等の CLI live wire 往復検査。

#### 超高効率計算＋収束加速の統合実行計画（Wave 0〜3 完了分 ＋ Phase A〜C 収束計画、2026-11-19 `91b19c4` 同期）

完成速度と並列スループットを最大化するため、**5 つの収束規律（B1〜B5）** を固定する：
- **B1**: Stage F1（残 D5 のみ）を先行確定し、いつでも外部公開可能な Alpha ラインを確保する。
- **B2 (`src/main.rs` 単一レーン規約)**: 31,219 行の `src/main.rs` 衝突を避けるため、ロジックは既存の専用モジュールへ厚く閉じ、**1 バッチ内で `src/main.rs` を編集するレーンは常に 1 つ**に固定して常時 3 並列をコンフリクトゼロで回す。
- **B3 (構造物バッチ受入工場)**: `tools/structure_capture.py` をバッチ化し、未受入定義を「0-diff 自動昇格」「共通カーネル原因クラスタ」「個別原因」に自動仕分けして波及の大きい 1 修正で複数構造物を同時 0-diff 化する。
- **B4 (2 階層高速ゲート＋自動ベンチ)**: サブエージェントは対象フィルタ＋`layers_check`/`debt_ratchet`/`provenance_check`、インテグレータは `TMPDIR=target/tmp` 分割実行＋`tools/perf_bench.py`（SHA-256・M1–M6・wall/CPU 中央値自動計測）で 600s タイムアウトと手動計測ロスを根絶する。
- **B5 (台帳同期)**: フェーズ完了ごとに `specs/` と運用文書を実態へ同期し、陳腐化による再調査ロスをゼロにする。

| フェーズ | Lane 1（`src/main.rs` 専任） | Lane 2（計算カーネル専任） | Lane 3（法務・構造物・独立モジュール専任） | 出口マイルストーン |
|---|---|---|---|---|
| **完了済 (Wave 0〜3: `8dad8c7`〜`91b19c4`)** | `P2-3-S2b〜d` + `S3` (`1ec70b0`, `0e532bb`, `3a3bb37`), `M3 P2b` (`0469ad9`) | `perf_bench.py` (`a9d8437`), Layer A1-a/b (`3cb6d24`, `b981704`), Layer A2-a/b/c (`c745772`, `785cdf5`, `4d28f01`), Layer A3-a/b/c (`2a4eaf4`, `b105d58`, `91b19c4`) | `igloo` + `P2-1-T2` (`2481b48`), `village_plains` 0-diff 確認 (`2a4eaf4`) | 3 次元配信・パレット化・AVX-512・DAG 並列基盤完了 |
| **Phase A（半完成回収＋逆転解消）** | **A-1**: `P2-3-S4` worktree 回収・`tests/dimension_wire.py` E2E 統合 | **A-2**: `Layer A3-a` 速度回収（`populate_parallel` の `TerrainShare`/owner キャッシュ共有） | **A-3**: Stage F1 `D5` worktree 回収（`export_public_branch.sh` + `public_export_test.py` + 文書同期） | Stage F1 完結、3 次元移動 E2E 完成、DAG 並列の実測高速化 |
| **Phase B（M3 性能完結）** | **B-1**: `Layer A4` 本体（`PalettedSection` P2c–f O(1) light skip + 共有圧縮チャンクキャッシュ） | **B-2**: `Layer A1-c`（`Cond::Biome(u128)` + `SurfaceProg`）＋ `P0-2` 到達性監査文書 | **B-3**: `src/entity_spatial.rs` ゼロアロケ化 ＋ PGO / `WARM=1` 対照ベンチマーク | M3 / Layer A1〜A4 性能目標達成、`WARM=1` 対照証明 |
| **Phase C（構造物・M2/P1 拡張＆最終ゲート）** | **C-1**: M2 残 wire 往復検査（戦闘・死亡/リスポーン・精錬・天候） | **C-2**: `P2-1-T3` 構造物バッチ工場（B3/B4：未検証構造物の採取と共通原因修正） | **C-3**: `src/spawner.rs` 一般 `BaseSpawner` 化 ＋ `P1-1..P1-4` → 同一 SHA 全ゲート監査（`P4-1`） | Stage F2 実用公開フォーク完全到達 |

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

現行ベースライン（[worldgen-cost](../evidence/gameplay/worldgen-cost/README.md)・[performance](../evidence/foundation/performance/README.md)）では、RSSはvanillaの約1/8（142対1,160 MiB）、定常tick本体は0.05〜0.17 ms/tick（20 TPS、予算超過0/100）である一方、移動時のCPUは1.6〜4.7倍重い。承認済みの5段階超軽量化計画（詳細と阻害要因対策は[チャンク生成の並列化](chunk-parallelism/README.md#m3-超軽量化-5段階計画p1p5承認済み)）でこれを詰める:

1. **P1 (`ChunkCols` / `BeardChunkData` 共有)**: まず **Slice P1a**（ロード非依存カウンタ M1〜M5 ＋ `TERRAIN_STORE_BOUND` A/B）で `full − NOFEAT`（+156%）の主因を確定したうえで、worker 側計算を owner 側 `col_c` 等へ安全に共有・還元する。
2. **P2 (常駐 `PalettedSection` ストア)**: 2026-10-20 に棄却した非圧縮配列キャッシュではなく、Single-value / Indirect / Direct の省メモリ section ストアを正本化し、light・anti-xray・feature 近傍読み・保存・送信で再計算と二重保持をなくす。
3. **P3 (`CARVERS` / `FEATURES` 並列化)**: 半径2の先行不変条件と3×3引き渡し（Horizon MIT `0564ab0` の概念参照）により、バイト決定性を保ったまま残る直列生成を並列化する。
4. **P4 (`DfProg` SIMD/バッチ評価と早期枝刈り)**: MIT `Soju06/hyperchunk`（`df_simd_avx2.c` 等）を先行例に、格子点の SIMD 評価と `material_rule` / `aquifer` の早期枝刈りを行う。
5. **P5 (ライト・M2物理/Mob・圧縮配信)**: Starlight 概念（全空気/不透明 section の O(1) スキップ、nibble 直接フラッド）、Lithium 概念（`VoxelShape`/AABB キャッシュ）、大量 Mob の構造的軽量化（連続配列による細粒度ロック撤去・空間グリッド索引による `O(N^2)` 排除・AI/経路探索の読み取り並列化）、`libdeflate` / zero-repack 送信を統合する。

**ライセンス境界:** コード流用は MIT/Apache-2.0 等の許容ライセンス（`Soju06/hyperchunk`, `valence-rs`, `MCHPRS`, `0564ab0` 以前の Horizon）に限る（[OSS survey](../evidence/foundation/oss-survey/README.md)）。C2ME・Starlight/ScalableLux・Lithium・`fb1135c` 以降の Horizon・Paper・Pumpkin・SteelMC 等の GPL/LGPL/AGPL 実装は**アルゴリズム概念の参照のみ**とし、コードを一切コピーしない。

**出口:** PLANのM3条件を代表負荷で満たし、再現手順・ノイズ・未対応機能を記録する。機能不足で仕事量が減った結果や、地形が異なる条件の旧測定を性能改善へ読み替えない。

## R4 — 対象revisionの統合・リリース判定

過去の統合線の検証を、今回の移行ブランチへ引き継ぎません。作業ブランチへのpushは保存・共有であり、mainへの統合や合格とは別です。

**出口:** PLANの合格条件と公開対象の機能範囲を満たしたうえで、統合候補そのものに対して、全体テスト、必要な並列回帰・wire・adapter・層/負債ゲート、変異テスト、独立レビューを完了し、ライセンスと既知の制約を確認する。通常のローカル統合では[承認された選択mutation方針](project-convergence/README.md#進行と検証の運用)を適用し、全campaignの監査結果と区別する。未実行・未開始のCIを緑とせず、ローカル実行とhosted CIを区別する。main更新やreleaseは、その判定とユーザーの指示を経て行う。

## R5 — 要求されたMod単位の互換拡張

順序と出口条件は[shim計画](mod-compatibility/shim-plan.md)（S0〜S5）に置く。最初は、mod jarが必要とする面を列挙してshimとの差分を出す `shimcheck`（S0）、次にNeoForgeの単純なMod（S1）、ブロック/アイテムを登録するMod（S2）、バッチ／スナップショット JNI 境界を持つ BlockEntity/tick 基盤（S3）の順に進める。shimの骨組みはユーザーのjarから手元で生成し、リポジトリには入れない。S3 基盤確立後は、別プロジェクトへ切り出さず `mc-spine` 本体上の「汎用オフロードカーネル群（接続グラフ・自動スリープ輸送/加工・Contraption 枠組み・リージョン負荷隔離）＋重量 Mod 固有の薄型加速アダプタ」へ段階拡張する。

**出口:** 対象Mod・版・API範囲・実行テストを特定し、その組み合わせに限った互換性を証明する。
## 横断規律

- [最小の因果差分から進める手順](worldgen-parity/investigation-workflow.md)を使う。既存captureの再利用→stage/writer特定→小さい実クラス対照→本番到達確認→compiling mutant→変更が固まった対象SHAの全gateと進める。原因の変わらない重い比較を繰り返さない。
- 比較前に、何が実行され、どのstageにあり、何がskipされたかを確かめる。値が変わらないだけで負の結果としない。
- source・固定具・実測binary・参照jarを結び付け、試験前後の正常系と変異後の復元を確認する。
- 古いcaptureの再記録を修正の代用にせず、意味の変化を説明してから更新する。
- 実装、部分parity、全体合格、性能改善、push、統合を別々に報告する。数値と履歴は証拠ページへ集約する。
