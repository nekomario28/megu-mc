# 実装・検証チェックリスト（`perf/worldgen-parallel`）

作成: 2026-11-06。基点: `48dfe73b4e1db64d5ce295f0e533d30780a8fc37`。
更新: 2026-11-21（同期基準: `fe9ef04`）。状態: **部分実装・限定検証済み。上位P0〜P4の完了は別判定**。Wave 5（B4 昇格・Save v2・protocol 767・execute・gamerules・block entities・fluids・M3 計測）、Wave B（CF-5 culling カーネル・`ChunkPacketCache`・broadcast culling・perf gate 拡張）、および Wave 6（CF-5 `client-app` 配線・P1-3 `LiveLightEngine`・Phase P0 決定性修正・P1–P3 エンジン群・Lane U1–U3・mod 橋渡し）を下記「Wave 5 同期受入」「Wave B 同期受入」「Wave 6 同期受入」に反映済み。
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

## Wave 5 同期受入（2026-11-21、HEAD `cfffac7`）

以下は `git log` と checkout 内の `evidence/` で確認できるコミット／証拠のみを根拠にした実行記録。上位 P0〜P4 の完了判定や `P4-1` 同一 SHA gate に代わるものではない。

| 同期項目 | 完了根拠（commit / evidence） | 残す境界 |
|---|---|---|
| Batch B4 3構造物 0-diff 昇格 | `1de42a3`（lily pad 支持規則、mushroom の cold skylight、mineshaft spawner claim、tree leaf 更新境界、fuzzy section palette の surface biome）＋調査 [`structure-batch-b4-hold-fix-20261121.md`](../evidence/gameplay/feature-parity/structure-batch-b4-hold-fix-20261121.md)（`8624a38`） | default と記録済み population order の両方で一致。以降の worldgen 変更（`a82df1a`, `cfffac7`）は dump SHA-256 で回帰確認済み |
| Save v2 snapshot／全 gamerule／mob属性／wire hook | `06c2a98`（UUID-keyed player state・spawn向き/targets・mob挙動・villager・raid、atomic world write）、`6aca648`（全 Save v2 gamerules と mob属性）、`ec8e31c`（gameplay mechanics と player save hook の本番配線）、`81476b9`（seed 保存 #74）／#73・#172・#218・#84・#75 | 逐次 input の往復/test は通過。並行更新下の snapshot 整合性は別契約 |
| Protocol 767 compression／RelMove／culling／`0x07` registry | `0bd0520`（compression & entity movement codec）、`4fed912`（compression・registry・visibility の本番 wire）、`74ae654`（datapack から Configuration registry を構築、`47d834d` で end-to-end 検証）、`7e87ef6` + `32b556c`（pure visibility／bounded chunk scheduling）、`c63a84e`（live chunk delivery budget と冗長 update の cull） | 新 wire は `P4-1` の同一 SHA 再収集対象 |
| `/execute` 全 pipeline | `35a075f`（全 subcommand AST/parser）、`f6e2d98`（parsed pipeline 評価）、`da6e5c0`（synthetic client chunk／execute wire expectation）、`61d2b2f`（selector matching の core 共有）／#180〜#189 | `execute in ... if block` の dimension-scoped readback は既知境界（[commands-datapacks-20261120.md](../evidence/gameplay/feature-parity/commands-datapacks-20261120.md)） |
| 全 51 GameRules | `525ac1e`（canonical 1.21.1 GameRule inventory、#169）、`90e7930`（部分 command coverage）、`6aca648`（全 Save v2 gamerule 永続化） | 全 51 の本番 wire 往復は `P4-1` で再確認 |
| 12 Block Entity 状態 helper | `140b089`（Vanilla 1.21.1 block entity state helpers、Refs #216） | P1-2 の template/NBT 初期化 parity は依然オープン |
| 流体伝播・waterlogging | `c1848f4`（伝播/waterlogging parity 拡張）、`8a2a23a`（solidifications と wash-away reader の本番配線）、`cfc2f31`（保存時の生成 fluid reservation 維持）／#106 | 全 fluid event trace の網羅は既存の限定契約の範囲 |
| M3 worldgen CPU／アロケーション | `a82df1a`（CPU/alloc 削減）、`cfffac7`（octave noise cache）＋ [`m3-warm-worldgen.md`](../evidence/perf/m3-warm-worldgen.md) | **`WARM=1` live CPU は mc-spine 16.82 s 対 Vanilla 4.23 s。equal-work live throughput／G3 50 ms budget は未検証**。`cfffac7` は end-to-end 利得なし |

`9ab10ca`／`9bfba2d` 等の同名コミットは HEAD の祖先ではないため、この台帳の完了根拠には使わない。

## Wave B 同期受入（2026-11-21、HEAD `a970d82`）

以下は `git log` と checkout 内ソース、および本 worktree での実実行結果のみを根拠にした記録。上位 P0〜P4 の完了判定や `P4-1` 同一 SHA gate に代わるものではない。

| 同期項目 | 完了根拠（commit / 実行結果） | 残す境界 |
|---|---|---|
| CF-5 Layer 1: `ChunkPacketCache`＋`broadcast_near` | `a970d82`：`src/connection_limits.rs` の `ChunkPacketCache`（512-entry FIFO、共有 `Arc<Vec<u8>>` plain フレーム＋閾値ごとの遅延 compressed フレーム、generation リセット）と `src/play_protocol.rs` の `cached_chunk_frame`/`chunk_packet_revision`（3×3 `chunk_versions`＋`map_chunk_sig`＋live block entity fingerprint）。`broadcast_near`/`broadcast_near_in` が `broadcast_radius`/`within_broadcast_range` 経由で Vanilla の厳密半径 `d² < r²`・同一 `DimId`・パケット由来半径（sound/particle/explosion）に限定 | `container_wire.py` live PASS（13.2 s、`target/debug/mc-spine`）。新 wire 挙動は `P4-1` の同一 SHA 再収集対象 |
| CF-5 `client-port` カーネル S1〜S6 | `4be29f7`（S1 `FaceVisibility` 6×6 ビットマスク＋S3 `DirectionBuckets`/`bucket_draw_mask`/`mc_spine_client_mesh_bucket_directions`）、`20fb3fa`（S6 `summarize_column`/`LodSummary`＋sealed-section culling）、`f7bed23`（S2 `GraphCuller` direction-monotonic BFS＋S4 `RenderRegion` 8×4×8 slab arena/`MultiDrawCommand`＋S5 `rebuild_dirty_ordered`/`resort_translucent`）。`cargo test --manifest-path client-port/Cargo.toml` 53 tests PASS | **`client-app` 配線は未完了**（Wave B1/B2）: `GraphCuller`/`RenderRegion`/`LodSummary`/`rebuild_dirty_ordered`/`resort_translucent` は `client-app/src/` から未参照。`client-app` は従来の `rebuild_dirty`＋per-section `sort_translucent` のまま |
| `tests/culling_perf_gate.py` 拡張と live gate | self-test に S1 `FaceVisibility`・S2 `GraphCuller`・S3 `DirectionBuckets`・S4 `RenderRegion` slab 再利用・S6 `LodSummary`・Layer 1 `ChunkPacketCache` の Python ミラーを追加。`--self-test` PASS、`--runs 20` PASS（19 metrics、全 quad/vertex カウント pin 一致・全 timing ceiling 内） | live モードは `mesh_bench`/`lod_bench` の出力のみ検査；S2/S4 の新カーネル専用ベンチ予算は今後の登録対象 |
| `client_fork_e2e.py` | `--self-test` PASS（offline、wrapper parser/tamper/redaction のみ） | フル E2E（ビルド＋Prism/mrpack export＋実サーバー往復）は未実施。release artifact 3種（`libmc_spine_client_mesh.so`/`libmc_spine_kernel.so`/`mc-spine-client`）は未ビルド |
| 仕様同期 | `specs/client-fork/sodium-culling-architecture.md` を HEAD `a970d82` の Layer 1/Layer 3 実装状態へ更新、`specs/ROADMAP.md` に Wave B 行を追加 | ドキュメントのみ；動作変更なし |

## Wave 6 同期受入（2026-11-21、HEAD `fe9ef04`）

以下は `git log` と checkout 内ソース、および `evidence/` の記録のみを根拠にした実行記録。**「エンジン＋codec＋単体テストが `main` に存在する」ことと「本番 `src/main.rs` live 経路への実配線・wire 検証済み」は厳密に区別する。** 上位 P0〜P4 の完了判定や `P4-1` 同一 SHA gate に代わるものではない。

| 同期項目 | 完了根拠（commit / 実行結果） | 残す境界 |
|---|---|---|
| CF-5 `client-app` 配線（Wave B1/B2 完了） | `ab06605`（S1–S5 パイプラインを `client-app/src/world.rs`/`render.rs`/`sim.rs` に配線: `GraphCuller` BFS、`RenderRegions` バッチ、`bucket_draw_mask`、`rebuild_dirty_ordered`/`resort_translucent`）、`32a59e4`（粗 LOD ABI 完了＋`client-app/src/lod.rs` `LodManager`） | S2/S4 専用ベンチ予算の perf gate 登録とフル `client_fork_e2e.py` E2E（ビルド＋Prism/mrpack export＋実サーバー往復）は未実施 |
| P1-3 永続 live light エンジン | `906eb2a`：`src/main.rs` の `LiveLightEngine` が次元ごとに `IncrementalLighting` を永続化。`WARM=1` live CPU は 16.670 s → **5.110 s（first warm）/ 3.290 s（second warm）**、記録済み pre-change 中央値 16.82 s、Vanilla 参照 4.23 s（[`m3-warm-worldgen.md`](../evidence/perf/m3-warm-worldgen.md)） | after 値は **host contention 下の逐次実行**で、interleaved binary A/B ではない。Vanilla 値は**再計測ではなく過去記録**。equal-work live throughput・G3 50 ms tick budget は未検証 |
| Phase P0 worldgen 決定性修正 | `ea14165`：並列 carve `aq_sched` parity（`ChunkCols::carve_sched` で build-end flag を保持・carve 入口で復元）、final-column `a2a_flat_corner_memo`、`m3_p1b_single_flight` bounds 更新、`tick_tests` saver starvation 修正 | 個別修正の広い回帰は `P4-1` 同一 SHA 再収集対象 |
| Phase P1–P3 エンジン群（`main` 到達） | `d17c771`：`src/server_config.rs`（`server.properties` 層）＋`src/server_ops.rs`（ops/ban/whitelist/stdin console）。`dcbbbea`：`src/anvil_io.rs`（Vanilla 1.21.1 Anvil import/export）。`4d4a13f`：`src/workstations_and_mounts.rs`（Stonecutter/Grindstone/Loom/Cartography＋mounts/equipment、protocol 767 codec）。`8811a89`：`src/teams_and_commands.rs`（`/team` `0x60`、`/tick` `0x71`/`0x72`、`/locate`、`/random`、`/return`、`.mcfunction` マクロ）。`96edcd3`：`src/recipes_and_stats.rs`（recipe book、statistics、tab list codec） | **エンジン＋codec＋単体テストは `main` に存在するが、P2/P3 コマンド・workstation・protocol packet の本番 live 配線（W1/W2/W3）は進行中 — マージされるまで live-wired 済みとは扱わない** |
| Lane U1 信頼性 | `57b4273`：`src/server_reliability.rs`（online-mode crypto/auth、Velocity forwarding、crash isolation、async autosave） | 同上：エンジン＋単体テスト済み、live wire 統合は進行中 |
| Lane U2 UX | `fe9ef04`：`src/operator_ux.rs`（safe spawn、status ping、LAN、console status、load smoothing） | 同上 |
| Lane U3 ＋ mod 橋渡し | `db66a9b`：36 個の `BUILTIN_SUPERSEDED_PERF_MODS`（`mod-adapter-core/src/fallback.rs`）＋ `ClientSettings`（`client-app/src/settings.rs`、`import_vanilla_options_txt`/`to_toml`）。`772c47c`：`src/aeronautics_and_dim_bridge.rs`（create-offload 経由の Aeronautics/dimension-mod runtime bridge） | **`ClientSettings` の本番 live 配線（C1）は進行中 — live-wired 済みとは扱わない** |
| その他の `main` 到達分 | `b22123e`/`4630269`（Create 戦略 spec・culling docs 同期）、`7a2877b`（`create-adapter` crate Phase C1-a）、`c255fa9`（contraption Save v2＋NeoForge registry sync codec）、`41770fa`/`3270c6f`/`1b7c7d8`/`e0fe8a3`（`mod-adapter-core` 抽出・multi-mod bench・4-tier classifier・`mc-spine:lod_v1` channel）、`c5c7788`/`9118323`/`db0b6a6`/`6c77b97`（dynamic dim・dimension-mod support・Aeronautics 6-DOF・create-offload bridge）、`7710277`/`344c6c5`（`server.toml`/anticheat・anti-xray occlusion）、`2011eb0`/`3f48042`/`ccc553d`（random ticks/gamerules・villager/raid/spawning・redstone sensor/bed-sleep/protocol 767 parity の live 配線） | mod-adapter 系の本番フック残件は従来どおり [shim-plan.md](mod-compatibility/shim-plan.md) で追跡 |

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
- [ ] **P0-2 1.21.1 density/surface/biomeの到達性監査** — `8d047a9` で `tools/p0_2_audit.py` の `Cond::Biome(u128)` アンカー検証・`< 128` 上限同期および 3 次元到達性監査スナップショット（[`evidence/foundation/audit-snapshots/p0-2-reachability-20261119.md`](../evidence/foundation/audit-snapshots/p0-2-reachability-20261119.md)、未対応・欠落ゼロ）を記録済み。残る `spawn_costs` と NaturalSpawner 実行経路の実クラス対照で完全閉鎖する。
- [x] **P0-3 再配布・出所の隔離判断（統合/公開ブロッカー — Stage F1 完了）** — `4b43735` で D1〜D4、`04f61a7` で D5、`bd06c8a` で本体 `GPL-3.0-or-later`（寄稿は `LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` のトリプルライセンス）・全ファイルの出所確定・`git filter-repo --invert-paths` による過去の削除済み不純ファイルの全履歴抹消を完了済み：
  - [x] **D1/D2 (`F1-1`, `4b43735`)**: `tools/vanilla_data/gen_vanilla_data.sh`（`--verify` 2,350 ファイル・SHA-256 照合）と `assets/block_states_1211.json` の jar 自前生成化を完了。
  - [x] **D3/D4 (`F1-2`, `4b43735`)**: 旧 `assets/pblocks.json`・`assets/properties.json` および `resourcepack.zip` を消費者切替の上で削除・全履歴抹消し、`tools/provenance_check.py`（`--self-test` 11/11 PASS、`--strict-public` 対応）をゲート追加。
  - [x] **D5 (`F1-3`, `04f61a7`, `6127c66`, `bd06c8a`)**: `tools/export_public_branch.sh`・`tests/public_export_test.py` 整備、不要な直下 `*.storyboard.png` 6枚の削除（および履歴抹消）、`datapacks/{modtest,rectest,spinetest,tagtest,testpack}` の自作確定、および `LICENSE` (`GPL-3.0-or-later`) / `COPYING.LESSER` (`LGPL-3.0-or-later`) / `COPYING.AGPL` (`AGPL-3.0-or-later`) / `Cargo.toml` / `NOTICE` / `THIRD_PARTY.md` / `README.md` / `CONTRIBUTING.md` / `LICENSING.md` の完全統一を完了。
- [ ] **P0-4 native比較契約の固定** — Fossilが**正に配置される**fixture、complete FULL chunks、必要halo、sourceの生成順、2回のnative採取の再現性を定義。artifactに入力・SHA・対象セル数・除外数・pending tick・writerを記録し、古い測定値の流用を防ぐ。

### P1 — Template finalization（**最優先の実装列**）

- [x] **P1-0 完了済みの限定基礎（再回帰が必要）** — template/tree共通 `src/voxel_faces.rs` の面走査、液体予約順、`src/scheduled_ticks.rs` のchunkごとの生成tick unpack、bounded full-cube Fossilのnativeオラクル。**これらは最終state・実配送・一般構造物の合格ではない**。
- [ ] **P1-1 隣接stateの最終確定** — [仕様](template-finalization/slices/01-neighbors.md)。既存 `src/structure_template.rs` / `src/tree.rs` / runtime ownerを確認し、`updateShapeAtEdge` と `updateFromNeighbourShapes` の変更セルと順序・予約tickをnativeと比較。waterlogged・connected・fragile・falling・mixed fluid、full-cubeでもshape更新を起こす例を含む。新しい第2の状態機械を作らず単一ownerへ統合。未知呼び出しはfail closed。
- [ ] **P1-2 Block Entity / Entity初期化** — [仕様](template-finalization/slices/02-initialization.md)。NBT付きtemplate・entityを使いnativeの受理/拒否、回転、座標/UUID、loot seed、finalizerを観察。 `src/structure_template.rs` と既存runtime NBT/保存ownerを接続し、**実在する初期化済みインスタンス**とpacket・保存/別プロセス再読込を比較。既存のNBT無し許容経路は新契約が通るまで維持し、保存形式の変更はこの段で行わない。
- [ ] **P1-3 生成tickの実行・配送** — [仕様](template-finalization/slices/03-delivery.md)。protoからruntimeへの受け渡しだけでなく、native `LevelChunkTicks` / `LevelTicks` とゲーム時刻・priority・sub-order・cross-chunk・chunk eligibility・上限の**実際に実行されたイベント列と最終状態**を比較。block/fluidを独立→統合、保存/再開と未ロードchunkを含む。既存`scheduled_ticks`とGEN-lock-free drainを保つ。
- [ ] **P1-4 実チャンクFossil受け入れ** — [仕様](template-finalization/slices/04-chunks.md)。実際にFossilが置かれるnativeの完全chunkを採取し、同一stage・source順・marginで全state、heightmap、block entity、pending tickを比較。続けてruntime tickとsave/reload、light・wireは**別のgate**で評価する。scripted-level oracleの緑だけで閉じない。

### P2 — 全WorldGenへ段階的拡張

- [ ] **P2-1 汎用template / processor / structure starts** — [仕様](worldgen-parity/README.md)。**34定義・16アルゴリズム型**すべてが `src/structure_pipeline.rs` に dispatch 先と scripted oracle TSV を持つ。残る作業は：
  - [x] **P2-1-T0 (`35cf12c`)**: `StructurePiece.placeBlock` の `SHAPE_CHECK_BLOCKS` マーク（template/jigsaw 経路）＋ `Feature.markAboveForPostProcessing` ＋ promotion 限定 `BushBlock` 除去（`village-taiga` 0 / 884,736 達成）。
  - [x] **P2-1-T1**: チャンク昇格時（`postProcessGeneration`）の非空流体セルに対する fluid tick 予約・配送接続（`src/main.rs` の `structure_template::register_fluid_ticker` / `promotion_fluid_tick` 登録済み）。
  - [x] **P2-1-T2 (`5fb816b`)**: `structure_pipeline::load_defs_inner` で1つの不正な構造物 JSON が全パイプラインを `None` に落とす欠陥を要素単位の fail-skip + ログに修正。
  - [x] **P2-1-T3 Phase C-2 / D-2b / Wave E2 / Wave 3/4 構造物受入拡大 (`ba6e2c3`, `d139913`, `8d51b1d`, `6a39df7`, `3f1cb4a`)**: Batch B1〜B6 合計23 fixtureが **23/23 MATCH, 0 DIFF**。Batch B5では `fortress` (0/98,304), `end_city` (0/65,536), `ruined_portal_nether` (0/49,152), `bastion_remnant` (0/65,536), `trial_chambers` (0/98,304) を昇格。B4 HOLDだった `buried_treasure`, `swamp_hut`, `jungle_pyramid` は `1de42a3` で default／記録済み population order の両方が 0-diff となり昇格（[調査](../evidence/gameplay/feature-parity/structure-batch-b4-hold-fix-20261121.md)）。
- [ ] **P2-2 残るconfigured featuresと実chunk差分** — `bonus_chest` および End の `minecraft:end_spike` は実装・native oracle 照合・4 mutation ゲート済み（`src/feature.rs`, `src/generation_contract_tests.rs`, `tools/mutation_check.sh`）。`sculk_patch` は `ebd19fd` で `spc_can_spread_from` entry predicateを修正し、記録済みnative population pass orderで `sculk_gate.py --require-match` が0/0/0 diff。その他の未対応featureと最新population順の範囲確認は継続。
- [x] **P2-3 三dimensionの通常サーバー経路（Stage F2 の中核）** — `MC_SPINE_WG_PRESET=end|nether` は offline 診断限定のため、以下の S0〜S4 で通常サーバーの3次元生成・保存・配信・移動を完成済み：
  - [x] **P2-3-S0 (`b38aa75`)**: `tests/protocol767.py` CLI ハーネスの次元対応（`dimension_type` の `min_y`/`height`/`has_skylight` 動的解釈）。
  - [x] **P2-3-S1 (`4576ec1`)**: `DimId` (`Overworld | Nether | End`) と次元別 `Gen` レジストリ（同一 seed 初期化）、`chunk_data` / `write_light_arrays` への `DimId` 配線。
  - [x] **P2-3-S2a (`526c338`)**: `WORLD` オーバーレイの `DimChunkKey = (DimId, i32, i32)` 化・次元別コンパニオンセーブ・旧 `world_overlay.bin` 後方互換（選択 mutation 4/4 proved）。
  - [x] **P2-3-S2b〜d (`77db4a6`, `0c0ecbb`)**: 残り 8 ストア（`S2b`: `CHESTS`/`FURNACES`/`CROPS`、`S2c`: `SIGNS`/`PENDING_BLOCKS`/`BACKOFF`/`PLUGIN_CELLS`、`S2d`: `POS_STORE`）の `DimId` キー化。
  - [x] **P2-3-S3 (`e2c8939`)**: Nether/End の 16 セクション（`0..256`）チャンク構築と `has_skylight == false` 時の `skyLightMask = 0` 配信。
  - [x] **P2-3-S4 (`c543c92`)**: `0x2b` Join Game / `0x47` Respawn による次元間遷移・ポータル移動・ログイン次元復元の wire 検証（`tests/dimension_wire.py` PASS、選択 mutation 8/8 proved）。

### P3 — M2 gameplay / M3性能（WorldGenと証拠を混ぜない）

- [x] **P3-1 M2観測可能なゲーム操作** — [合格範囲](PLAN.md)・[Horizon計画](horizon-gameplay/README.md)。Slice 3a〜3d（チェスト/樽/エンダーチェスト・遅延 LootTable・waterlogged chest、`3374380`）、Slice 4a〜4c（衝突・流体高/水流押し・押し合い/空間グリッド、`150e53a`）完了。
  - [x] **Slice 4d (`e15a7ce`)**: `LivingEntity.travel` の水中/溶岩移動・浮力・落下減衰と Nether `ultraWarm` 溶岩押し流し倍率（`0.0023333333333333335`）。
  - [x] **一般 1.21.1 `BaseSpawner` + `SpawnPotentials` (`0c88980`)**: `src/spawner.rs` の重み付きポテンシャル・カスタムスポーン規則・NBT 往復。
  - [x] **Phase C-1 M2 実 wire 往復検査 (`2a68722`)**: 戦闘・死亡/リスポーン・2×2 クラフト/かまど精錬・天候/昼夜永続化の CLI 実 wire 往復検査（`tests/m2_gameplay_wire.py` PASS、`tests/verify.sh` 統合済み）。
  - [x] **Phase D-1 ランタイム Lock 順序・EID 衝突・次元別流体/作物修正 (`ff8addf`)**: `FURNACES`/`CROPS`/`Mob.pos` のロック順序逆転解消、`load_world` 保存ドロップと Mob EID の衝突防止、Nether/End の次元別プレイヤー流体（溺水/溶岩）＆作物成長修正（`d1_` 3 tests + `m2_gameplay_wire.py` & `dimension_wire.py` PASS）。
- [ ] **P3-2 生成の並列化と性能gate（M3 P1〜P5 + 超高効率計算 Layer A1〜A4）** — [仕様](chunk-parallelism/README.md)。実装完了（Phase D-2 のホットパス修正と代表負荷 `WARM=1` 対照の最終判定は `P4-1` で統合確認）：
  - [x] **M3 P1a (`7c00617`) / P1b (`5f01327`)**: 負荷非依存カウンタ（M1〜M5）＋ラウンド単位 `TerrainShare`・single-flight `starts`/`rings`・`Arc` `terr_c` により、`full` CPU +88% ペナルティを完全解消。
  - [x] **M3 P2a (`src/paletted.rs`) / P2b (`0ef6a60`) / P2c (`30d0346`)**: 常駐 `PalettedSection` モジュール（`src/paletted.rs`）、`SECTION_STORE`・zero-repack チャンク配信、`PalettedSection::uniform_state()` O(1) uniform fast-path。
  - [x] **M3 P3a (`61d516d`) / Layer A3-a/b/c (`b1beec5`, `c0478a1`, `c99fff9`) / A3-a キャッシュ共有 (`482e81b`)**: Carver dense map + `OriginGrid` + `carve_batch`、Feature `u128` バイオームマスク、バッチ pre-carve + 依存 DAG 並列 `populate_parallel` ＋ ワーカー `col_c`/`chunk_biomes` 共有（全次元 SHA-256 ビット完全一致）。
  - [x] **M3 P4a (`3e1ba0b`) / Layer A1-a/b/c (`101b9f7`, `cd3abf9`, `482e81b`) / Layer A2-a/b/c (`79a2002`, `2733c13`, `7b3917f`)**: Aquifer `AqChunkGrid` 化、`beard`/stronghold 遅延化、2D コーナーメモ＋AVX2/AVX-512 `DfProg`/noise、lazy surface mats、`Cond::Biome(u128)`＋`Rule::Seq` 直接反復。
  - [x] **M3 P5 & Layer A4 (`d4fa504`, `0c88980`, `30d0346`, `d06032e`)**: 配信パレット書込 -89%、sky spill -52〜63%、floor cache -88%、`EntitySpatialGrid` 単一マップ化、512-entry `MapChunkCache`、`tools/pgo_build.sh`（PGO `full j=8` `2.971s -> 2.656s`、-10.6%、全 dump SHA-256 ビット完全一致）。
  - [x] **Wave 3/4 評価と起動修正**: Issue `#41` palette churn（`01badc1`）および Issue `#42` region storage（`eec10bf`）はそれぞれ評価結果 NO-GO。`WARM=1` 初期81-chunk login timeoutは `6e4e534` + `80617f3` で修正し、`WARM=1 bash tools/load_compare.sh 20 0` が `LOAD ok packets=3265 chunks_moved=90`。当該測定では mc-spine 17.510s CPU / 357.7 MiB / 3 threads、Vanilla 4.610s CPU / 1138.1 MiB / 66 threads。これは記録された負荷の結果で、P4-1全gateの代替ではない。
  - [x] **Wave 5 M3 CPU／アロケーション削減 (`a82df1a`, `cfffac7`)**: [M3 計測](../evidence/perf/m3-warm-worldgen.md) に記録。オフライン full 8-worker CPU `8.748 → 4.821 s`、4×4 アロケーション `10,762,088 → 565,305`（−94.7%）、full/no-feature/terrain の全 dump SHA-256 と cross-seed/dimension のブロック・heightmap が byte 一致。`cfffac7` の octave-noise cache は five-round A/B で full 1-worker CPU −1.00%／8-worker +0.57% の小さく混在した差にとどまり **end-to-end 利得なし**。
  - [ ] **M3 残り出口（未完了）**: `WARM=1` live CPU は P1-3 永続 `LiveLightEngine`（`906eb2a`）で 16.670 s → **5.110 s（first warm）/ 3.290 s（second warm）** に改善（記録済み pre-change 中央値 16.82 s、baseline 20.56 s、Vanilla 参照 4.23 s）。ただし after 値は host contention 下の**逐次実行**で interleaved A/B ではなく、Vanilla 値は過去記録のまま。live driver の chunk 配信数が異なるため equal-work 比較は未実施、G3 50 ms tick budget も未検証。統合候補 SHA での back-to-back は `P4-1` に委譲。

### R5 — Mod 互換層（NeoForge 1.21.1 S0〜S3+）

- [x] **NF-Arch（設計確定 `3efb811`）**: [neoforge-architecture.md](mod-compatibility/neoforge-architecture.md)（5 レイヤ構成・`DimId` 対応・バッチ JNI・`shimcheck` 起動前拒否）。
- [x] **NF-0 (S0 `shimcheck` `a42ebb2`)**: `ShimCheck.java`（ASM `9.10.1` + `neoforge.mods.toml` 解析）＋ `tools/shimcheck.py` ＋ `tests/shimcheck_test.py`（33 tests PASS）。
- [x] **NF-1 (動的 Bootstrap & Dual Event Bus `6d3a03d`)**: `ShimCheck` ゲート付き複数 Mod 動的ディスカバリ・コンストラクタ注入・`Bus.GAME` / `Bus.MOD` 振り分け・`Dist.CLIENT` 除外・`tests/neoforge_bootstrap_test.py`（計 45 tests PASS）。
- [x] **NF-2 Java S1 6イベント & `MutationBuffer` (`0ce02ac`)**: `BreakEvent` / `EntityPlaceEvent` / `PlayerLoggedInEvent`・`OutEvent` / `ServerChatEvent` / `RegisterCommandsEvent` + 再入上限 4 の `MutationBuffer` + `ShimCheck` S1 面拡張（`tests/neoforge_s1_events_test.py` 含む 50 tests PASS）。
- [x] **NF-2b (S1 Rust Host Bridge & CAS 適用 `56d6f5e`)**: `src/landing_types.rs`（`MutationOp` / `S1EventOutcome` / `decode_mutation_batch`）および `src/java_compat.rs`（`apply_s1_outcome` & エンコード済み JNI ヘルパ）で Java 側 `MutationBuffer` を Rust 側へ返却・CAS 適用（`tests/neoforge_s1_events_test.py` 51 tests + Rust 5 tests PASS）。
- [x] **NF-3 (S2 `DeferredRegister` + 名前ベース永続化 `9267492`)**: `DeferredRegister` / `RegisterEvent` / 決定的 Freeze（動的 Block `26,684〜` / Item `1,333〜`）＋ `ModRegistrySnapshot` 名前パレットコーデック（Py 56 + Rust 7 PASS。残件：実オーバーレイ保存＆クライアント `0x07` 同期接続）。
- [x] **NF-4 (S3 Direct `ByteBuffer` バッチ JNI `e403691`)**: `DirectBatchBridge.java` + `S3BatchBridge` 固定 `256 KiB` バッファ（1,024 BE 更新を 1 往復で往復、世代失効＆depth 5 全体ロールバック検証、Py S3 6/6 + Rust 10/10 PASS。残件：実 tick ループからの呼び出し）。
- [x] **NF-5 (S4 NeoForge `1d104ae`)**: `CustomPacketPayload`, `IItemHandler` capability lookup, dynamic registry `0x07` entry sync（`tests/neoforge_s4_network_test.py` PASS）。S4設計 `6f55e1f`、fail-closed `ShimCheck` diagnostics、BlockEntityTicker と Configuration-phase `PayloadRegistrar` closureを含む。
- [x] **S4/Fabric/Mixin surface audit (`733db18`)**: 7-JAR監査（[top-mods-surface-20261120.md](../evidence/mod-compatibility/top-mods-surface-20261120.md)）。4つの非-Mixin NeoForge pattern JARは `gameplay` 8/0、`content` 12/0、`automation` 10/0、`network` 12/0 で **COMPATIBLE**、member refs 43/44 supported。結果は監査対象JARに限定。
- [x] **Issue `#53` JVM評価 (`0b5603f`)**: [評価記録](../evidence/mod-compatibility/ristretto-eval-20261120.md) の結論は Ristretto NO-GO、Embedded HotSpot 21 JNI GO（起動 +70.84 ms、12s RSS +55.11 MiB、EventBus p50 40 ns）。
- [x] **S5 `mod-offload/` (`634a270`, `47d49ff`, `3bc3c71`, `956b56c`, `9eb56ac`)**: `ConnectionGraph`, `SettledTransport`, `KineticNetwork`, `FluidNetwork`, `RecipeProcessor`, `ContraptionGrid`, `RegionScheduler`、C11/C++17 ABI `mc_spine_mod_offload.h`、4+1 thread region isolation（11.50 µs/tick）。JNI↔Rust C-ABI計測: decode 6.282 µs / 0.210 µs、settled 10k-machine step 80 ns vs Java full scan 2.044 µs（25.5x）。
- [x] **Fabric S0〜S2 (`58e7fb2`, `65c4944`)**: Fabric 1.21.1 `FabricShimCheck`, `EnvFilter`, Mixin/AccessWidener gate、Runtime Lifecycle/Event Callbacks、Freeze-Locked Registry Adapter（`tests/fabric_shim_test.py` 21/21 PASS）。
- [ ] **NeoForge/Fabric runtime production hooks**: 完了済みS1〜S4対応範囲を `src/main.rs` の実tick/network経路へ接続する残作業は [shim-plan.md](mod-compatibility/shim-plan.md) に従って追跡する。

### R6 — クライアントフォーク計画と Server-Client 共通カーネル（`mc-spine-client`）

- [x] **CF-0（Server-Client共有Kernel公開・クレート抽出 `51fdd39`, `c6a81b8`）**: `client-kernel/` standalone `rlib`/`cdylib` crate と C-ABI `mc_spine_kernel.h`、Protocol 767 server↔C-ABI prediction wire suite。600 byte-exact sections、36 position samples（walk/sprint/jump/slab/water）、0.000000 drift、unexpected `0x40` resync 0。
- [x] **CF-1（ヘッドレス 1.21.1 決定論的シミュレータ & E2E bot client）**: [tests/client_prediction_wire.py](../tests/client_prediction_wire.py) の実 `mc-spine` wire予測検証に加え、Vanilla差分ハーネス [parity_client.py](../tools/client_diff/parity_client.py) は8/12 movement/slab/block/fluid casesがlive Vanilla 1.21.1と0.0 drift。残る4/12は非-movement packet-detail mismatchesとして記録（`736fa3d`, `47df6fa`）。
- [x] **CF-2（Client Port描画・増分lighting・Greedy Meshing）**: `client-port/` Greedy Mesher + AO + fluid surfaces + 24-section column meshing + packed 16-byte vertex VBO + translucent sorting と Protocol 767 wire bridge（`0018bac`, `d384fb8`, `8f7a520`）。`768 -> 3` quad reduction（256.00x）、5,208.55 µs/column。増分lighting `src/lighting.rs` directional slab/stair face occlusion・settled emitter-removal parityは15/15 live Vanilla snapshots byte-exact（`6598fe6`, `906ec67`; `tests/lighting_parity_test.py` PASS）。
- [x] **CF-3（`mods/` 投入型 Rust Integrated Server 第 1 段 JAR パッケージ完了 `8199d14`）**: [build_integrated_mod.py](../tools/client_launcher/build_integrated_mod.py) で Fabric/NeoForge 両対応 `mc-spine-integrated.jar` ＋ `IntegratedServerBridge` ループバック起動・監視・クリーン終了を実装（`client_launcher_test.py` 18/18 PASS。残件：実ゲーム Client Mod ライフサイクル・Direct `ByteBuffer` FFI 接続）。
- [x] **CF-4（Launcher bundle exporter — `e46f6b0`）**: Prism Launcher / Modrinth `.mrpack` / Official Launcher bundle exporter と SHA-256 integrity gate（`tools/client_launcher/export_launcher_bundle.py`, `tests/client_launcher_test.py` 25/25 PASS）。
- [ ] **CF-5（グラフオクルージョンカリング＋粗 LOD 可視化アーキテクチャ — カーネル＋app 配線完了・E2E 未完了）**: [sodium-culling-architecture.md](client-fork/sodium-culling-architecture.md) の 3 層契約に基づき、Layer 1（server）に `ChunkPacketCache` 共有 `0x27` フレーム＋`broadcast_near` Vanilla 半径/次元 culling（`a970d82`、`container_wire.py` live PASS）、Layer 3 の `client-port` カーネルに S1 `FaceVisibility`・S2 `GraphCuller`・S3 `DirectionBuckets`・S4 `RenderRegion`・S5 `rebuild_dirty_ordered`/`resort_translucent`・S6 `LodSummary`（`4be29f7`, `20fb3fa`, `f7bed23`、53 unit tests PASS、`culling_perf_gate.py` self-test＋live `--runs 20` PASS）を実装し、`client-app` への配線（Wave B1/B2 — `GraphCuller`/`RenderRegion`/`LodManager`/新スケジューラ採用）も `ab06605`・`32a59e4` で完了。**残件**: S2/S4 専用ベンチ予算の perf gate 登録、フル `client_fork_e2e.py` E2E（release artifact 3 種のビルド＋実サーバー往復）。

### R7 — ゲームプレイ完全性（M2→M4：`GP-0`〜`GP-7`）

> 以下の各項目は「純粋エンジン＆ワールド／プロトコル 767 アダプタ（Phase D / E Wave 1 完了）」と「本番 `src/main.rs` 実配線・wire 検証（Phase E Wave 2〜3）」を分けて管理する。

- [x] **GP-0（Entity Framework & 全 130 エンティティ型データ駆動レジストリ — エンジン＆主要配線完了 `89669a8`, `d6f17ac`）**: `src/entity_registry.rs` に protocol 767 全 130 `EntityType`（`0..=129`）を一元化し、`name_to_etype`・`mob_dimensions`・`default_mob_hp`・`/summon` 初期 HP を接続済み。
- [x] **GP-1（1.21.1 `GoalSelector` & A* `PathNavigation` — live wiring `f218379`）**: `GoalSelector` とA* `WorldPathAdapter`を `mob_tick` で起動。
- [x] **GP-2（1.21.1 `NaturalSpawner`, Mob Cap, `SpawnCosts`, Despawn）**: Overworld の pack spawning と despawn を維持。Nether は次元別 Mob census、バイオーム／要塞の選択、配置・明暗、cap・距離および `SpawnCosts` を本番 tick に接続。[`gp2_nether_spawn_wire.py`](../tests/gp2_nether_spawn_wire.py) で soul sand valley の自然スポーンと予算による密度制限を確認。`SpawnCosts` 状態 `fd0c863`、Nether spawn rules `2cfd249` で energy budget まで本番配線済み。
- [x] **GP-3（1.21.1 レッドストーン伝播エンジン — live wiring `e279113`）**: wire/torch/repeater/comparator/piston/lamp/door/hopper/dropperを `src/main.rs` に接続（`tests/gp3_gp4_gp5_wire.py` PASS）。
- [x] **GP-4（村人 AI・職業 POI・取引・gossip・Raid — live wiring `e279113`）**: Villager profession POI、`0x2d` Merchant Offers / `0x2f` Select Trade、restock/demandを本番接続（wire suite PASS）。
- [x] **GP-5（エンチャント台・金床・醸造・鍛冶・空腹・XP — `e279113`）**: 3-slot enchant options、Anvil repair/rename、Brewing stand tick、Smithing tableに加え `/xp set` と `FoodData::tick` を接続し、wire検証済み。
- [x] **GP-6（残存 Mob 群のグループ別行動・戦闘 — live wiring `f218379`）**: 14 mob behaviorsを `mob_tick` に接続（例: Zombie A*, Cow wheat follow, Slime split, Enderman water escape, Blaze fireball, Piglin zombification）。
- [x] **GP-7（Advancements・Scoreboard・WorldBorder・`/fill`・`/clone`・Commands/Datapacks）**: GP-7 gameplay wiring `7b60c93` (`gp5_gp7_wire.py` 6/6 PASS) に加え、`861f4c7`/`df347b6` で Protocol 767 Brigadier `0x10` login Commands、`/op`/`/deop`/`/reload`/`/datapack enable|disable`、`0x0b`/`0x0f` tab suggestions、`ops.json` persistence、`.zip` datapacksを実配線（`command_brigadier_ops_wire.py` PASS、`command_datapack_wire.py` 81/81 PASS）。`/execute` は `35a075f` で全 subcommand AST/parser、`f6e2d98` で parsed pipeline 評価、`da6e5c0` で wire expectation を完了（#180〜#189）。既知境界: Scoreboard任意表示名/sidebar外スロット・BE clone、`execute in ... if block` の dimension-scoped readback。

### P4 — 統合前のゲート（自動ではmergeしない）

- [ ] **P4-1 同一SHAの全gateと独立レビュー（未完了）** — `tests/verify.sh` 完走、native oracle、本番wire、mutation、serial/parallel、adapter、layers/debt、Fossil/structure/feature・dimension回帰の結果を同一revisionで収集。hosted CIを使えない場合は未実行と明記。Wave 5 で追加された Save v2／compression・`0x07` registry／`/execute`／gamerules／block entities／fluids／B4 昇格の wire と M3 `WARM=1` back-to-back も同じ `cfffac7` 系候補 SHA で再収集する必要がある。過去の[Issue #7](https://github.com/nekomario28/mc-spine/issues/7)はクローズ済み。
- [x] **P4-2 配布物・ライセンス・履歴クリーンアップ（Stage F1 & F2 完了 `bd06c8a`）** — P0-3（D1〜D5）、本体 `GPL-3.0-or-later` ＋ 寄稿 `LGPL-3.0-or-later / GPL-3.0-or-later / AGPL-3.0-or-later` トリプルライセンス確定、`git filter-repo --invert-paths` による過去の削除済み不純ファイルの全履歴抹消、`main` への統合完了。

## 次の着手

[ROADMAPの直近の状態と出口](ROADMAP.md)を参照する。Wave 3/4・Wave 5 の完了済みレーンと Wave B（CF-5 culling カーネル＋Layer 1 フレームキャッシュ＋`client-app` 配線）・Wave 6（P1–P3 エンジン群・Lane U1–U3）の同期済み範囲を反映済み。次の統合出口は P2/P3 コマンド・workstation・protocol packet の本番 live 配線（W1/W2/W3）、`ClientSettings` live 配線（C1）、`WARM=1` live CPU の equal-work 再計測（Vanilla 4.23 s 対比、現行 5.110/3.290 s は逐次実行の参考値）、P1 template/Fossil 残件の追跡と `P4-1` 同一SHA Release Gate。
下記の作成時点の「未実行」を、現在の未実装一覧や最新gate結果に読み替えない。

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
