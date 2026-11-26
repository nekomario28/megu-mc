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

**現在の状態（2026-11-22、main `e08d21e1` 時点）:** Wave 3/4 の Track 1〜4 完了に加え、Batch B4 の 3 構造物（`buried_treasure`/`swamp_hut`/`jungle_pyramid`）が `1de42a3` で default・記録済み population order の両方で 0-diff 昇格（[調査記録](../evidence/gameplay/feature-parity/structure-batch-b4-hold-fix-20261121.md)）。Save v2 の snapshot／全 gamerule／mob属性永続化（`06c2a98`, `6aca648`, `ec8e31c`; #73/#172/#218/#84/#75）、Protocol 767 の compression／RelMove／culling／`0x07` registry（`0bd0520`, `4fed912`, `74ae654`, `c63a84e`, `7e87ef6`, `32b556c`）、`/execute` 全 pipeline（`35a075f`, `f6e2d98`, `da6e5c0`; #180〜#189）、全 51 GameRules（#169/`525ac1e`, `90e7930`, `6aca648`）、12 Block Entity 状態helper（#216/`140b089`）、流体伝播・waterlogging（`c1848f4`, `8a2a23a`, `cfc2f31`; #106）、`GP-2` の `SpawnCosts`／Nether spawn（`fd0c863`, `2cfd249`）も完了。さらに HEAD `a970d82` では client-fork 可視化アーキテクチャ（CF-5）の `client-port` 側カーネル S1〜S6（`FaceVisibility`・`GraphCuller`・`DirectionBuckets`/`bucket_draw_mask`・`RenderRegion` MultiDraw・`rebuild_dirty_ordered`/`resort_translucent`・`LodSummary`）が実装・単体テスト済み（53 tests PASS）となり、Layer 1 に `ChunkPacketCache`（`0x27` フレームの reader 間共有・revision ゲート）と `broadcast_near` の Vanilla 半径/次元 culling が追加された（[sodium-culling-architecture.md](client-fork/sodium-culling-architecture.md)）。`tests/culling_perf_gate.py`（self-test＋live `--runs 20`）、`tests/container_wire.py`、`tests/client_fork_e2e.py --self-test` は本 worktree で全て PASS。**`client-app` 側の配線（Wave B1/B2: `GraphCuller`/`RenderRegion`/`LodManager`/新スケジューラの採用）も `ab06605`・`32a59e4` で完了**。さらに Wave 6 で Phase P0 の worldgen 決定性修正（`ea14165`: `ChunkCols::carve_sched`、final-column `a2a_flat_corner_memo`、`m3_p1b_single_flight` bounds、`tick_tests` starvation）、Phase P1–P3 のエンジン群（`src/server_config.rs`/`server_ops.rs`（`d17c771`）、`src/anvil_io.rs`（`dcbbbea`）、`src/workstations_and_mounts.rs`（`4d4a13f`）、`src/teams_and_commands.rs`（`8811a89`）、`src/recipes_and_stats.rs`（`96edcd3`））、Lane U1（`57b4273` `src/server_reliability.rs`）・U2（`fe9ef04` `src/operator_ux.rs`）・U3（`db66a9b`: 36 built-in superseded perf mods＋`ClientSettings`）、Aeronautics/dimension-mod runtime bridge（`772c47c` `src/aeronautics_and_dim_bridge.rs`）が `main` に到達した。**これらはエンジン＋codec＋単体テスト済みであり、本番 live 配線とは別に区別する**（本番配線は後続の W1〜W5/C1 コミットで完了 — 下記）。 M3 `WARM=1` live CPU は P1-3 永続 `LiveLightEngine`（`906eb2a`）により 16.82 s → **5.110 s（first warm）/ 3.290 s（second warm）**（Vanilla 参照 4.23 s、[`evidence/perf/m3-warm-worldgen.md`](../evidence/perf/m3-warm-worldgen.md) — after 値は host contention 下の逐次実行で interleaved A/B 非対象、Vanilla 値は過去記録）に改善。

その後 HEAD `32ed673` までに Wave 6 の残件だった本番 live 配線が全て完了した: **W1 `4fec043`**（P3-B/server-ops コマンド・権限レベル・`.mcfunction` マクロ・stdin コンソール・Anvil CLI）、**W2 `1194685`**（Stonecutter/Grindstone/Loom/Cartography workstation メニュー・mounts）、**W3 `473650a`**（login admission・vehicles・recipes/stats・64 セクション超 light mask）、**W4 `971fcac`**（U1 本番配線: `run_isolated_connection` crash isolation・Velocity modern forwarding・online-mode AES-128-CFB8・rich status ping＋`server-icon.png`）、**W5 `39a09b1`**（U2 本番配線: `AsyncSaveWorker`＋バックアップローテーション・safe spawn・LAN announcer・`AutoLoadBalancer`・console `status`）、および **C1 `5590ce1`**（`ClientSettings` の CLI/render/world/LOD/sim 配線）。ルートクレート警告 0 のまま、「エンジン＋単体テスト存在」と「本番 live 配線」の区別はこれらで閉じた。

さらに **Two-Target アーキテクチャ**（`1.21.1` / protocol 767 / data version 3955 ＋ 最新 vanilla `26.3` / protocol 777 / data version 5023 / pack `(121, 0)`、`megu-core` の `TargetVersion` が唯一の正本）と **現行 12 独立クレートの無環 DAG**（11 クレートは外部依存ゼロ、12 番目の `client-gpu` は独立 `Cargo.lock` に winit/wgpu を隔離、依存辺は [architecture-crates.md](../docs/architecture-crates.md)、`[profile.fast]` は継続）が `main` に成立した: N1a `0d804dc`（[multi-version-architecture.md](multi-version-architecture.md) 設計＋VersionPack tool）、N1c `68c17b9`、N2a `0c92514`（multi-version codec/router＋client wire 採用）、N2c `7aaa3fa`（kernel multi-version registry loader）、`f5855fa`（standalone `megu-proto`）、H1 `e4d6f7a`（proto 777＋pure-std SRV DNS）、H2 `747d3f2`、H3 `94e31f6`（64-shard `DimensionState`/`ServerContext`＋DataFixer）、H5 `45b1276`（`server-files`）、H6 `1bfbf33`（deep DataFixer＋決定的 region ticking）、H8 `248bfa4`（`megu-core` crate）、H10 `b70b23e`（`STV1` CRC32 snapshot＋`ServerDirectory` facade）、H11 `bd75d4e`、H13 `e89b258`（client-kernel/client-port の core 採用＋26.3 tall dims）、M2 `c951fa8`、M2b `e53eadf`、M3 `c955c72`、M3b `7b32766`、V2 `83c50e5`（live W1/W3 wire suite・`version_pack` gate・`--emit-version-pack`）。`bbe4821` で client-app は live SRV 解決・dual 767/777・`--version-dir` kernel loading を獲得し、**`38f2ba2` で 26.3 (777) の chunk/block/chat/disconnect wire surface が完結して mock-server E2E（handshake→login→config→chunk→PNG 描画）が成立**、`778e0e1` で `--ping` status query＋常時 `FrameBudgetGovernor`、`d4ac2ad` で `1.21.1 ↔ 26.3` world migration engine＋`megu-world-migrate` CLI（`STV1` snapshot）、**`32ed673` で pure-Rust zero-dep `megu-mod-check`**（Modrinth `.mrpack`/CurseForge pre-flight、`tools/mod_launcher.py --modpack` と byte-identical JSON）を獲得した。release 系では R1 `e5e98a1`（決定的 `tools/release_bundle.py`＋export 時の `*_1211` 完全除去）、R2 `c259307`（`docker/Dockerfile` clean multi-stage＋[release-checklist.md](../docs/release-checklist.md)＋release-gate wiring）、H7 `3a7a362`（`tools/parity_cache.py`＋`dev_check` crate/disk 診断）が完了。

**直近の完了と残件:** V3（`tests/protocol767.py` の `0x03` compression＋live wire、`2c765dcb`）、H16（`client-kernel` 128 セクション light、`248d47e4`）、H17（standalone `megu-init`、`13500e4a`）は完了済み。G1（`tools/mutation_check.sh` の `tests/gate_scripts.py` 向け anchor 再設定）、N2b（`server_config`/`save_v2`）、M2 全 vanilla parity、M3 equal-work live proof は未完了。S2C Play の live emission は 107/124（86.3%）、dead mechanism は 0、C2S 58/58・GameRules 51/51・commands 89/89・behavioral matrix 30/30、限定 Vanilla live diff は 12/12 MATCH。12 番目の `client-gpu` は `winit` 0.30.13 + `wgpu` 30.0.1 の実ウィンドウ／オフスクリーン描画を実装し、RX 7800 XT（RADV）で非空の Vulkan offscreen render を確認した記録がある。宣言型 Magic（`MAG1`）・VFX（`VFX1`）・skeletal animation（`ANM1`）・combat motion（`MOT1`）の各エンジン／wire／client host と HUD を実装。2-client 7 項目 Alpha smoke の 7/7 PASS 記録、3 OS×6 バイナリの release workflow がある。第三者 modpack 実 E2E、外部 shaderpack の描画互換性、M2 全 worldgen は未検証。P1 template/Fossil 残件、S2/S4 専用ベンチ予算、フル `client_fork_e2e.py` E2E、その他の未完了チェック、equal-work live throughput・G3 50 ms tick budget、および同一SHAの `P4-1` Release Gate も継続中であり、詳細と証拠は[実装・検証チェックリスト](implementation-checklist.md)および各 `evidence/` を参照。

## 直近の実行順（2026-11-22 再同期、HEAD `a079e1d6`）

長期の R0〜R7 は維持し、4 トラック＋独立ファイル所有境界に沿って最大並列で進める。Wave 5 で Save v2／protocol 767／command／gameplay の本番配線を、Wave 6＋W1〜W5/C1 で P2/P3 コマンド・workstation・protocol packet・U1/U2 エンジン・`ClientSettings` の本番 live 配線を、Wave 7 で Two-Target（`1.21.1`/`26.3`）アーキテクチャ・当時の 10 クレート無環 DAG・release tooling を、Wave 8 で strict parity audits・限定 live Vanilla 差分・外部 shaderpack 基盤を同期済み。現行は 12 独立クレート（11 外部依存ゼロ＋`client-gpu`）と Magic/VFX/Animation/Combat-Motion の宣言型処理を追加済み。[実行台帳](implementation-checklist.md)で「アダプタ／プロトタイプ実装済み」と「本番サーバー (`src/main.rs`) 実配線・wire 検証済み」を厳密に区別する。

| 優先トラック | 対象 | 次の出口 / 同期状態 |
|---|---|---|
| **Collaborator レーン (`GP-2` — 完了)** | `src/main.rs` + `src/natural_spawner.rs` + `src/mob_despawn.rs` | Overworld の pack spawning／cap・距離・配置・明暗・despawn と Nether の次元別 census、バイオーム／要塞リスト、配置・明暗、`SpawnCosts` エネルギー予算を本番 tick に接続済み（`fd0c863`, `2cfd249`、[`gp2_nether_spawn_wire.py`](../tests/gp2_nether_spawn_wire.py)）。未完了項目なし。 |
| **Track 1 (Worldgen, Light & Perf: `P1` / `P2` / `#41..#43`)** | Batch B4 昇格・`P1` Template/Fossil・`WARM=1` M3 | Batch B4 の 3 構造物が `1de42a3` で default／記録順ともに 0-diff 昇格。M3 オフラインは full 8-worker CPU 8.748→4.821 s、アロケーション −94.7%（`a82df1a`、`cfffac7`、[M3 計測](../evidence/perf/m3-warm-worldgen.md)）が、**`WARM=1` live CPU は `906eb2a` の永続 `LiveLightEngine` で 16.82 s → 5.110 s（first warm）/ 3.290 s（second warm）へ改善**（Vanilla 参照 4.23 s、逐次実行・host contention caveat、equal-work・G3 budget は未検証）。`cfffac7` の noise cache は end-to-end 利得なし。P1-1/P1-4 残件と `P4-1` は未完了。 |
| **Track 2 (Mod 互換: `R5 NF-5 / S4〜S5 / #53`)** | NeoForge/Fabric 互換基盤・監査・オフロード・JVM評価 | 既完了（`NF-5` S4 `1d104ae`、S4/Fabric 監査 `733db18`、`#53` JVM 評価 `0b5603f`、S5 `mod-offload/` `634a270`、Fabric S0〜S2 `58e7fb2`/`65c4944`）。Wave 5 で compression／`0x07` registry の本番 wire を追加（`0bd0520`, `4fed912`, `74ae654`, `47d834d`）。 |
| **Track 3 (Client Fork: `R6 CF-0〜CF-4` + CF-5 culling)** | 共有Kernel・Vanilla対照・Mesher・Launcher・可視化カーネル | 既完了（`CF-0` `51fdd39`/`c6a81b8`、`CF-2` `0018bac`/`d384fb8`/`8f7a520`、`CF-1b` `736fa3d`/`47df6fa`、`CF-4` `e46f6b0`）。限定 live Vanilla 1.21.1 差分は 12/12 MATCH（`eddeea93`、旧 8/12 記録を更新）。CF-5 は `client-port` カーネル S1〜S6 が実装・テスト済み（`4be29f7`, `20fb3fa`, `f7bed23`、`culling_perf_gate.py` self-test＋live PASS）で、**`client-app` 配線（Wave B1/B2）も `ab06605`・`32a59e4` で完了**。残件は S2/S4 専用ベンチ予算とフル `client_fork_e2e.py` E2E。 |
| **Track 4 (Gameplay 本番実配線: `R7 GP-0..GP-7`・Commands/Datapacks)** | `src/main.rs` 接続＋実 wire 検証 | `/execute` 全 pipeline（`35a075f`, `f6e2d98`, `da6e5c0`; #180〜#189）、全 51 GameRules（#169/`525ac1e`, `90e7930`, `6aca648`）、12 Block Entity helper（#216/`140b089`）、流体（#106/`c1848f4`, `8a2a23a`）、Save v2 snapshot（#73/#172/#218/#84/#75/`06c2a98`）を完了。従来の GP 配線・Commands／datapacks（`861f4c7`, `df347b6`）も維持。 |
| **Release Gate (`P4-1`)** | 同一 SHA での `verify.sh` 拡張・代表負荷 M3・公開エクスポート確認 | 新 wire／Mod／Client／Save／execute テストの `tests/verify.sh` 統合、`WARM=1` M3 back-to-back、選択 mutation、`layers_check`/`debt_ratchet`/`provenance_check` 同一 SHA 収集。**未実施**。 |

### 公開可能フォーク（Public Fork Release）への収束実績と Phase D 計画（2026-11-20）

#### Stage F1: 法的にクリーンな Experimental / Alpha 公開フォーク（P0-3 / D1〜D5 完結）
- **F1-1 (`P0-3 D1/D2`: 公式 `server.jar` からの全資産ローカル生成化 — 完了 `4b43735`)**:
  - `tools/vanilla_data/gen_vanilla_data.sh`（＋`verify_pack.py`、`--verify` 2,350 ファイル照合）と `assets/block_states_1211.json` の jar 自前生成化を完了。
- **F1-2 (`P0-3 D3/D4`: GPL 同値アセット・出所不明ファイルの完全抹消と fail-closed ゲート — 完了 `4b43735`)**:
  - 旧 `assets/pblocks.json`・`assets/properties.json` と `resourcepack.zip` を消費者ごと切替・除去し、`git filter-repo --invert-paths` で過去全履歴からも完全抹消済み。`tools/provenance_check.py`（`--self-test` 11/11 PASS、`--strict-public` モード付き）で再混入を常時ブロック。
- **F1-3 (`P0-3 D5`: ライセンス統一とクリーン公開エクスポート — 完了 `04f61a7`, `bd06c8a`, 2026-11-22 LGPL 統一)**:
  - 本体および全マニフェストの配布ライセンスを **`LGPL-3.0-or-later`**（[COPYING.LESSER](../COPYING.LESSER)・[LICENSING.md](../LICENSING.md)）に統一済み。また、Mojang 由来生成物を一切含まない単一ルートコミット公開ツリーを出力する `tools/export_public_branch.sh`（`tests/public_export_test.py`）も完備。

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
  - Batch B4 の残り 3 構造物（`buried_treasure`, `swamp_hut`, `jungle_pyramid`）は `1de42a3` で lily pad 支持規則・mushroom の cold skylight 判定・mineshaft spawner claim・tree leaf 更新境界・fuzzy section palette の surface biome 読取りを修正し、default／記録済み population order の両方で 0-diff 昇格（[調査記録](../evidence/gameplay/feature-parity/structure-batch-b4-hold-fix-20261121.md)）。P1 template/Fossil 残件は継続。
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
| **完了済 (Phase D: `ff8addf`〜`5522bc2`)** | **D-1** (`ff8addf`: Lock 逆転・EID 衝突・次元流体修正) ＋ **PR `#55`/`#56`/`#57`** (`5522bc2`: GP-2 夜間スポーン距離・cap・地面配置の本番接続) ＋ `GP-0`/`GP-5` 部分接続 (`d6f17ac`) | **D-2b** (`d139913`: `valid_biome` quart 判定＆Mansion 基礎) ＋ **D-2a** (`f5c0e8e`: `Rule::Seq` ゼロアロク・`ctx.g.sea`・`OriginGrid` 疎バッチ保護) | **NF-2 Java S1** (`0ce02ac`, 50 tests PASS) ＋ **R6 CF-4/CF-1** (`0ea0c3e`) ＋ **R7 `GP-0〜GP-7` 全 8 純粋エンジン＆パリティ修正** (`d6f17ac`) | 全監査指摘閉鎖 ＋ R5 S1 / R6 ランチャー / R7 全 8 エンジン完了 |
| **完了済 (Phase E Wave 1: `56d6f5e`〜`9889a5c`)** | **PR `#61`** (`577891f`: GP-2 夜間スポーン明暗チェック統合) ＋ **PR `#63`** (`013fcd0`: `layers_check` timeout 引上げ) | **E1 並列アダプタ完全統合 (`9889a5c`)**: `GP-1` (`c71b6ca`, PR `#59`), `GP-3` (`9889a5c`, PR `#62`), `GP-4` (`eb4eb8b`, PR `#58`), `GP-5` (`a66e388`, PR `#60`), `GP-6` (`8c2271d`), `GP-7` (`1ac8b7e`, PR `#64`) — 計 181 単体テスト全 PASS | **NF-2b** (`56d6f5e`: Rust↔Java S1 Host Bridge & CAS 適用, 51 Py + 5 Rust PASS) ＋ **CF-1/CF-3** (`153a0fd`: 流体/次元予測 & `--megu-integrated` ループバック基盤, 12 launcher + prediction wire PASS) | 全 6 Gameplay アダプタ＆PR `#58〜#64` 完全統合・NeoForge S1 Rust 往復完結・Client 予測拡張完了 |
| **完了済 (Phase E Wave 2 & NF-4: `7b60c93`〜`8199d14`)** | **E2-1 (`7b60c93`)**: `GP-7` (Border/Scoreboard/Advancements/`fill`/`clone`) ＋ `GP-5` (`/xp set`, `FoodData::tick`) 本番実配線（`gp5_gp7_wire.py` 6/6 PASS） | **E2-3 (`8d51b1d`)**: HOLD 再検証＆Batch B4（`ocean_ruin_warm` **0-diff 昇格**, 18/18 MATCH） | **E2-2 (`9267492`)**: `NF-3` S2 `DeferredRegister`＋名前永続化 ／ **E3-2 (`e403691`)**: `NF-4` S3 `DirectByteBuffer` バッチ JNI（1,024 件/1 往復） ／ **E2-4 (`8199d14`)**: `CF-0` 共有カーネル契約＋`CF-3` `megu-integrated.jar`（18 tests PASS） | GP-7/5 実通信開通・NeoForge S2/S3 完成・Integrated JAR 成立 |
| **完了済 (Phase E Wave 3/4: `main` `df347b6`)** | **E3-1a/E3-1b**: `GP-1`/`GP-3`/`GP-4`/`GP-5`/`GP-6` 本番配線、GP wire suites PASS。 | **E3-3/E3-7/E3-8/E3-9/E3-10**: 増分ライト、#41/#42評価、Batch B5 23/23、sculk修正、WARM=1 timeout修正と比較実行。 | **E3-5/E3-6/E4-1/E4-2/E4-4**: JVM評価、互換監査、NeoForge S4/S5、Fabric S0〜S2完了 ／ **E3-4a/E3-4b/E4-13/E4-17/E4-23/E4-20**: CF-0/2/1b完了。 | Wave 3/4各レーンを台帳同期。P1残件など個別未完了項目と **`P4-1` Release Gate** は継続（`GP-2` は Wave 5 で完了を反映）。 |
| **完了済 (Wave 5: `1de42a3`〜`cfffac7`)** | **E5-1**: Save v2 snapshot／全 gamerule／mob属性／player save hook（`06c2a98`, `6aca648`, `ec8e31c`, `81476b9`; #73/#172/#218/#84/#75）。 | **E5-2**: Batch B4 3構造物 0-diff 昇格（`1de42a3`, 調査 `8624a38`）＋ M3 CPU/アロケーション削減（`a82df1a`, `cfffac7`, [証拠](../evidence/perf/m3-warm-worldgen.md)）。 | **E5-3**: `/execute` 全 pipeline（`35a075f`, `f6e2d98`, `da6e5c0`; #180〜#189）、compression／RelMove／culling／`0x07` registry（`0bd0520`, `4fed912`, `74ae654`, `c63a84e`, `7e87ef6`, `32b556c`, `47d834d`）、全 51 GameRules（#169/`525ac1e`, `90e7930`）、12 Block Entity（#216/`140b089`）、流体（#106/`c1848f4`, `8a2a23a`, `cfc2f31`）。 | Wave 5 レーンを台帳同期。**`WARM=1` live CPU は `906eb2a` の永続 `LiveLightEngine` で 16.82 s → 5.110/3.290 s へ改善**（Vanilla 参照 4.23 s、逐次実行 caveat）。P1残件と `P4-1` Release Gate は継続。 |
| **完了済 (Wave B: CF-5 culling `4be29f7`〜`32a59e4`)** | `ChunkPacketCache` による `0x27` フレーム reader 間共有＋revision ゲートと `broadcast_near` の Vanilla 半径/次元 culling（`a970d82`）；`container_wire.py` live PASS（gamemode 修正 `594d247` 含む）。 | `client-port` カーネル: S1 `FaceVisibility`＋S3 `DirectionBuckets`（`4be29f7`）、S6 `summarize_column`/`LodSummary`＋sealed culling（`20fb3fa`）、S2 `GraphCuller`＋S4 `RenderRegion`＋S5 ソートスケジューラ（`f7bed23`）；53 unit tests PASS、`culling_perf_gate.py` self-test＋live `--runs 20` PASS。 | `client-app` 配線（Wave B1/B2）完了: `ab06605`（`GraphCuller`/`RenderRegion`/`bucket_draw_mask`/`rebuild_dirty_ordered`/`resort_translucent`）＋`32a59e4`（`LodManager`/`active_lod_columns`）；`client_fork_e2e.py --self-test` PASS（offline）。 |
| **完了済 (Wave 6: `b22123e`〜`fe9ef04`、live 配線は W1〜W5/C1 `4fec043`〜`5590ce1` で完了)** | random ticks/gamerules・villager/raid/spawning・redstone sensor/bed-sleep/protocol 767 parity の live 配線（`2011eb0`, `3f48042`, `ccc553d`）。 | Phase P0 worldgen 決定性修正（`ea14165`: `ChunkCols::carve_sched`、final-column `a2a_flat_corner_memo`、`m3_p1b_single_flight` bounds、`tick_tests` starvation）。 | P1–P3 エンジン `src/server_config.rs`/`server_ops.rs`（`d17c771`）、`src/anvil_io.rs`（`dcbbbea`）、`src/workstations_and_mounts.rs`（`4d4a13f`）、`src/teams_and_commands.rs`（`8811a89`）、`src/recipes_and_stats.rs`（`96edcd3`）；Lane U1 `src/server_reliability.rs`（`57b4273`）、U2 `src/operator_ux.rs`（`fe9ef04`）、U3 `db66a9b`；`src/aeronautics_and_dim_bridge.rs`（`772c47c`）；mod-adapter/create-adapter 基盤（`41770fa`, `7a2877b`, `c255fa9`, `3270c6f`, `1b7c7d8`, `e0fe8a3`, `c5c7788`, `9118323`, `db0b6a6`, `6c77b97`, `7710277`, `344c6c5`）。 | **W1〜W5＋C1 の本番 live 配線は完了済み**（`4fec043`, `1194685`, `473650a`, `971fcac`, `39a09b1`, `5590ce1` — Wave 7 行参照）。`P4-1` 同一 SHA gate 継続。 |
| **完了済 (Wave 7: Two-Target / Horizon DAG `0d804dc`〜`32ed673`)** | **W1〜W5＋C1 本番配線**: W1 `4fec043`（P3-B/server-ops コマンド・権限レベル・`.mcfunction` マクロ・stdin コンソール・Anvil CLI）、W2 `1194685`（Stonecutter/Grindstone/Loom/Cartography・mounts）、W3 `473650a`（login admission・vehicles・recipes/stats・64 セクション超 light mask）、W4 `971fcac`（U1: crash isolation・Velocity forwarding・online-mode AES-128-CFB8・rich status ping＋`server-icon.png`）、W5 `39a09b1`（U2: `AsyncSaveWorker`＋backups・safe spawn・LAN announce・load smoother・console `status`）、C1 `5590ce1`（`ClientSettings` CLI/render/world/LOD/sim）。ルートクレート警告 0 を維持。 | **Two-Target `1.21.1`（767/3955）＋ `26.3`（777/5023、pack `(121,0)`）**: `megu-core` `TargetVersion` が唯一の正本（H8 `248bfa4`、H11 `bd75d4e`、H13 `e89b258`）、当時の 10 クレート無環 DAG（`core`→`proto`→`client-app`、`core`→`state-store`/`server-files`/`mod-adapter-core`/`client-kernel`、leaf `client-port`/`mod-offload`、`create-adapter`）、`[profile.fast]`。N1a `0d804dc`（[multi-version-architecture.md](multi-version-architecture.md)）、N1c `68c17b9`、N2a `0c92514`、N2c `7aaa3fa`、`f5855fa`、H1 `e4d6f7a`（pure-std SRV DNS）、H2 `747d3f2`、H3 `94e31f6`、H5 `45b1276`、H6 `1bfbf33`、H10 `b70b23e`（`STV1` snapshot）、M2 `c951fa8`、M2b `e53eadf`、M3 `c955c72`、M3b `7b32766`、V2 `83c50e5`、`bbe4821`（dual 767/777＋`--version-dir`）。 | **バイナリスイート＋release tooling**: `38f2ba2` 26.3→PNG client E2E（mock-server handshake→chunk→描画）、`778e0e1` `--ping`＋`FrameBudgetGovernor`、`d4ac2ad` `1.21.1↔26.3` migration engine＋`megu-world-migrate`、`32ed673` pure-Rust zero-dep `megu-mod-check`（`.mrpack`/CurseForge pre-flight）、`3a7a362` `tools/parity_cache.py`、R1 `e5e98a1` 決定的 `tools/release_bundle.py`、R2 `c259307` `docker/Dockerfile`＋[release-checklist.md](../docs/release-checklist.md)、H18 `release_bundle.py` に repeatable `--extra-binary`（multi-binary suite を同一 fail-closed guard で同梱）。 | **完了:** V3（`2c765dcb`）、H16（`248d47e4`）、H17（`13500e4a`）。**Wave 7 時点のオープン:** G1、N2b、S2C Play 残件、M2 全 worldgen、M3 equal-work live、第三者 modpack E2E、実 GPU ウィンドウ、`P4-1` 同一 SHA gate。現在の GPU 実装状況は上記の状態を参照。 |

| **完了済 (Wave 8: 厳格監査・shaderpacks `eddeea93`〜`a079e1d6`)** | **Strict Vanilla audit** (`ceaf3849`): C2S Play 58/58、GameRules 51/51、command roots 89/89、behavioral matrix 30/30。限定 live Vanilla 1.21.1 差分 12/12 MATCH（`eddeea93`）。 | **Strict NeoForge/Fabric 1.21.1 self-audit** (`ceaf3849`): overall 53/53、各 loader 50/50。**Debt ratchet** lock/rwlock unwrap 19/19 上限、wall-clock reads 0。 | **外部 shaderpack＋native WGSL**: OptiFine/Iris 1.7–1.11+ loader（`469a15f5`）、WGSL＋旧 GPU safe fallback（`11e82fb8`）、CLI/TOML/RenderGraph（`001de852`）、GLSL 120–460 reflector＋`client-ui` 設定画面（`a079e1d6`）。`dh_shadow`/`dh_terrain`/`dh_water`・`megu-shader.toml`、作者ガイド `008a42c9`。第三者 shader 同梱なし。 | **現行更新:** live S2C Play 107/124（86.3%）、dead mechanism 0。実 `winit` + `wgpu` window/offscreen は `client-gpu` に実装済み。M2 全 worldgen、M3 equal-work live、Wave 3 第三者 modpack E2E／shaderpack 描画互換性、`P4-1` 同一 SHA gate は未完了。 |

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
6. **残る M3 出口作業（未完了）**: Phase D-2（`Rule::Seq` ゼロアロケーション化・`OriginGrid` 疎バッチ保護）に加え、`a82df1a` の CPU／アロケーション削減（オフライン full 8-worker CPU `8.748 → 4.821 s`、アロケーション `10,762,088 → 565,305`（−94.7%）、全 dump SHA-256 と cross-seed/dimension ブロック・heightmap が byte 一致）を反映済み。`cfffac7` の octave-noise cache は [M3 計測](../evidence/perf/m3-warm-worldgen.md) で小さく混在した差（full 1-worker CPU −1.00%、8-worker +0.57%）にとどまり **end-to-end の利得なし**。**`WARM=1` live CPU は P1-3 永続 `LiveLightEngine`（`906eb2a`）で 16.670 s → 5.110 s（first warm）/ 3.290 s（second warm）に改善**（記録済み pre-change 中央値 16.82 s、Vanilla 参照 4.23 s）。ただし after 値は host contention 下の逐次実行で interleaved A/B ではなく、Vanilla 値は過去記録のまま。equal-work live throughput と G3 50 ms tick budget は未検証。統合候補 SHA での最終 back-to-back（`P4-1`）は未実施。

**ライセンス境界:** 本体および全マニフェストは 2026-11-22 に **`LGPL-3.0-or-later`** 単一表記へ統一済み（[LICENSING.md](../LICENSING.md)）。**C2ME (`MIT`)**、**Lithium (`LGPL-3.0`)**、`Soju06/hyperchunk` (`MIT`)、`valence-rs` (`MIT`)、`MCHPRS` (`MIT`)、`Azalea` (`MIT`) は `LGPL-3.0-or-later` とライセンス互換である。外部からコード・手法を取り込む場合は帰属とライセンスを [THIRD_PARTY.md](../THIRD_PARTY.md) / [NOTICE](../NOTICE) に明記し、第三者の `GPL-3.0-only` / `AGPL-3.0-only` コードはファーストパーティファイルへ直接コピーせずアルゴリズム概念・ブラックボックス比較または専用モジュール境界で分離する。また PLaMo 3 Translate 等の外部 AI モデル重みはリポジトリ・配布物へ同梱せず、利用者の明示同意後に公式配布元からローカル取得する。

**出口:** PLANのM3条件を代表負荷で満たし、再現手順・ノイズ・未対応機能を記録する。機能不足で仕事量が減った結果や、地形が異なる条件の旧測定を性能改善へ読み替えない。

## R4 — 対象revisionの統合・リリース判定

過去の統合線の検証を、今回の移行ブランチへ引き継ぎません。作業ブランチへのpushは保存・共有であり、mainへの統合や合格とは別です。

**出口:** PLANの合格条件と公開対象の機能範囲を満たしたうえで、統合候補そのものに対して、全体テスト、必要な並列回帰・wire・adapter・層/負債ゲート、変異テスト、独立レビューを完了し、ライセンスと既知の制約を確認する。通常のローカル統合では[承認された選択mutation方針](project-convergence/README.md#進行と検証の運用)を適用し、全campaignの監査結果と区別する。未実行・未開始のCIを緑とせず、ローカル実行とhosted CIを区別する。main更新やreleaseは、その判定とユーザーの指示を経て行う。

## R5 — 要求されたMod単位の互換拡張

順序と出口条件は[shim計画](mod-compatibility/shim-plan.md)（S0〜S5）に置く。最初は、mod jarが必要とする面を列挙してshimとの差分を出す `shimcheck`（S0、完了 `a42ebb2`）、次に動的ブートストラップと Dual Event Bus（NF-1、完了 `6d3a03d`）、NeoForgeの単純なMod（S1 / NF-2）、ブロック/アイテムを登録するMod（S2 / NF-3）、バッチ／スナップショット JNI 境界を持つ BlockEntity/tick 基盤（S3 / NF-4）の順に進める。shimの骨組みはユーザーのjarから手元で生成し、リポジトリには入れない。S3 基盤確立後は、別プロジェクトへ切り出さず `megu` 本体上の「汎用オフロードカーネル群（接続グラフ・自動スリープ輸送/加工・Contraption 枠組み・リージョン負荷隔離）＋重量 Mod 固有の薄型加速アダプタ」へ段階拡張する。

**出口:** 対象Mod・版・API範囲・実行テストを特定し、その組み合わせに限った互換性を証明する。

## R6 — クライアントフォーク計画と Server-Client 共通カーネル（`megu-client`）

サーバー（`megu`）で確立した **Minecraft Java 1.21.1 (protocol 767) 厳密準拠のデータ駆動カーネル**をそのままクライアントフォーク（および公式 Launcher / Prism Launcher でワンクリック起動できる軽量 Rust クライアント／Integrated Server）へ展開するため、以下の 5 段階（`CF-0`〜`CF-4`）をロードマップに組み込む：

1. **CF-0（Server-Client 共有コアの公開 — `layers_check.sh` 互換の Zero-Drift Shared Kernel）**:
   - **モジュール層ゲート維持の設計原則**：`tools/layers_check.sh`（`src/main.rs` 起点の 44 modules / 111 edges 静的依存検査）を壊さないよう、いきなり別ディレクトリへファイルを移動して Cargo Workspace 分割するのではなく、`src/*.rs` のファイル配置と層ポリシー（`evidence/foundation/module-layers/policy.json`）を保ったままライブラリターゲット（`rlib` / `cdylib`）から以下の純粋カーネルモジュールを公開する：
     - **ブロック状態・ボクセル衝突・面支持**：`src/block_registry.rs` (`26,684` states、`FULL`/`CENTER`/`RIGID` 面支持)、`src/entity_collision.rs` (1.21.1 AABB 衝突解決・step-up)、`src/voxel_faces.rs`。
     - **流体物理・エンティティ移動**：`src/entity_fluid.rs` (`LivingEntity.travel` 水流・溶岩流・浮力・落下減衰)、`src/entity_push.rs`、`src/entity_spatial.rs`。
     - **チャンクストレージ・ライト・NBT・コマンド**：`src/paletted.rs` (`PalettedSection` wire zero-repack)、`src/lighting.rs` (`0x27`/`0x2a` ライトマスクと nibble 伝播)、`src/nbt.rs`、`src/brigadier.rs` (`0x11` コマンド木とクライアント側サジェスト評価)。
   - **メリット**：一般的なクライアント実装（Torch や Azalea 等）で最大のバグ源となる「サーバーとクライアントの物理・当たり判定・流体・ブロック予測の微妙なズレ（ラバーバンド現象・位置ズレ）」が、**同一の 1.21.1 Rust カーネルを共有することで構造的にゼロ**になる。
2. **CF-1（ヘッドレス 1.21.1 決定論的シミュレータ & E2E ボットクライアント）**:
   - 現行の `tests/protocol767.py` / `tests/m2_gameplay_wire.py` / `tests/dimension_wire.py` を発展させ、Rust 共有カーネル上で動く **ヘッドレス 1.21.1 クライアントランタイム**を構築する。
   - サーバーから受信した `0x27` Level Chunk With Light、`0x09` Block Update、`0x5a` Set Entity Motion、`0x47` Respawn をクライアント側 `PalettedSection` と `entity_collision` / `entity_fluid` で毎 tick ローカル予測実行し、Vanilla 1.21.1 サーバーおよび `megu` サーバーの双方に対して「クライアント側予測位置 vs サーバー確定位置の誤差 0」を自動検証する。
3. **CF-2（クライアント側レンダリング・ライティング・メッシュ生成パイプラインとの接続）**:
   - Issue `#40`（解決済）・`#43`（Torch 増分ライティング比較）で整理した境界に従い、ファーストパーティファイルへ外部の `AGPL-3.0-only` コードを混ぜず（トリプルライセンス `LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` の双方向性を維持）、`megu` の `PalettedSection`（`uniform_state()` O(1) スキップ・`hidden_filler`・`non_air` カウンタ）と `src/lighting.rs` から直接 Greedy Meshing / Chunk Section VBO を生成するクライアント描画層を接続する。
4. **CF-3（`mods/` 投入型 Rust Integrated Server — `megu-integrated.jar` / `cdylib`）**:
   - 既存の Fabric / NeoForge クライアント環境（Sodium / Iris シェーダー等をそのまま利用）の `mods/` に JAR 1 個を入れるだけで、シングルプレイ／LAN ホスト時の重い Java Integrated Server を **JAR 内蔵の Rust ネイティブ `libmegu` (`cdylib`) + 共有メモリ / Direct `ByteBuffer` FFI** に完全置換する。
   - 描画・UI は使い慣れた既存クライアント Mod 資産を活かしつつ、ワールド生成（SIMD / Rayon DAG 並列）・構造物・照明・Redstone・流体・エンティティ Tick を 100% Rust で爆速実行する。
5. **CF-4（Default Launcher / Prism Launcher ネイティブ起動パッケージ — 極薄 Bootstrapper JAR & Wrapper Command）**:
   - **公式 Launcher / Prism / Modrinth App 全対応の Custom Version JSON + 極薄 Bootstrapper JAR（約 20 KB）**：ランチャーが起動する `mainClass` から `--username` / `--uuid` / `--accessToken` / `--assetIndex` / `--gameDir` を受け取って即座に Rust ネイティブバイナリへ引き渡し（`execve` 置換または JVM 即時終了）、**起動 50ms 以降は JVM メモリ 0・GC カクつき 0 の 100% 純粋な Rust ネイティブプロセス**として動作させる。
   - **Prism Launcher `Wrapper command` / `.mrpack` 直接起動**：Prism Launcher のラッパーコマンド機能に `megu-wrapper` を指定することで、**一瞬たりとも `java` プロセスを起動せず**ランチャーのアカウント認証・アセット管理だけをそのまま流用して 0.1 秒で Rust クライアントを起動する。

**出口:** サーバーとクライアントフォークが 1.21.1 の `block_registry` / `paletted` / `entity_collision` / `entity_fluid` / `brigadier` を二重実装せず共有し、ヘッドレス予測検証（CF-1）、`mods/` 投入型 Integrated Server（CF-3）、および公式/Prism ランチャーからの 100% Rust ネイティブ起動（CF-4）が Vanilla 1.21.1 と完全互換で動作する。

## R7 — ゲームプレイ完全性と全エンティティ・機構拡張（M2→M4：`GP-0`〜`GP-7`）

現状 [PLAN.md](PLAN.md) の M2 範囲（「基本mob」中心）を超えてサバイバル・マルチプレイ・建築・レッドストーン回路・全エンティティを完全動作させるため、規約 B2（`src/main.rs` 競合回避）に従い専用スロットを `include!` で分離したうえで以下の 8 段階（`GP-0`〜`GP-7`）を実装し、[ゲーム要素の実装計画](gameplay/README.md)の未着手 slice（06〜14）と直結させる：

1. **GP-0（Entity Framework & 全 130 エンティティ型データ駆動レジストリ — `src/entity_registry.rs`）**:
   - 1.21.1 protocol 767 の全 130 `EntityType`（ID `0..=129`、名前、幅・高さ・`eye_height`、`MobCategory`、既定 `max_health`、`fire_immune`、`pushable`、`client_tracking_range`、`update_interval`）を一元定義し、`entity_collision.rs`・`spawner.rs`・`default_mob_hp` の散在手書きテーブルを統合する。
2. **GP-1（1.21.1 `GoalSelector` & `PathNavigation` / A* 経路探索 — `src/mob_ai.rs`）**:
   - `WalkNodeEvaluator` 準拠の `BlockPathTypes` 判定（固体・段差・落下 `<= 3` ブロック・対角線角切り防止・水/溶岩/炎/サボテン回避ペナルティ）、A* `PathFinder`、排他制御フラグ（`MOVE | LOOK | JUMP | TARGET`）を持つ `GoalSelector` / `TargetSelector`、代表ゴール群（`MeleeAttackGoal`, `RangedBowAttackGoal`, `SwellGoal`, `TemptGoal`, `BreedGoal`, `WaterAvoidingRandomStrollGoal`, `LookAtPlayerGoal`, `NearestAttackableTargetGoal`, `PanicGoal`）を実装する。
3. **GP-2（1.21.1 `NaturalSpawner` 完成 — `src/natural_spawner.rs`）**:
   - `MobCategory` 7 分類の `mob_cap = capacity * spawnable_chunks / 289`、`SpawnCosts` のエネルギー予算、ディメンションごとの光条件、群れスポーンを実装する。最寄りプレイヤーから `<= 24` ブロックではスポーンを拒否する。despawn 対象はカテゴリ上限（通常128ブロック、`WATER_AMBIENT` は64ブロック）を超えれば即時、32ブロック超かつ上限以内では `no_action_time > 600` のとき `1/800` の確率で despawn する。
4. **GP-3（1.21.1 レッドストーン伝播エンジン — `src/redstone.rs`）**:
   - 強電源（DirectSignal）／弱電源（Signal）モデル、`RedStoneWireBlock`（`0..=15` 減衰・4方向接続・段差昇降・上ブロック遮断）、`RedstoneTorchBlock`（反転・2GT 遅延・60GT 内 8 回反転の burnout 保護）、`RepeaterBlock`（2/4/6/8GT 遅延・横ロックダイオード）、`ComparatorBlock`（比較/減算モード・コンテナ充填率信号 `getAnalogOutputSignal`）、およびピストン（Quasi-Connectivity 含む）・ランプ・ドア/トラップドア・ホッパー・ディスペンサー/ドロッパーへの駆動接続を実装する。
5. **GP-4（村人 AI・職業・取引・ゴシップ・襲撃）**:
   - 村人（Villager）の POI（職業ブロック・ベッド）紐付け、Merchant レベル/オファー・補充・需要価格変動・取引 XP、アイアンゴーレム召喚・ゴシップ、および不吉な予感・襲撃（Raid）波状フェーズを実装する。
6. **GP-5（プレイヤー高度機構：エンチャント台・金床・醸造・鍛冶・空腹・XP）**:
   - 本棚カウント付きエンチャントテーブル 3 択生成と乱数シード、金床修理/エンチャ合成（`repair_cost`・名前変更）、醸造台ポーション遷移と `effects.rs` 拡張、鍛冶台テンプレート強化、経験値オーブ回収、および `foodExhaustionLevel` 消費の精密化を実装する。
7. **GP-6（残存 Mob 群のグループ別実装：Nether / End / 水棲 / 中立・ペット / ボス）**:
   - `GP-0` + `GP-1` の上に、Nether 系（Blaze・Ghast・Piglin 物々交換・Hoglin・Wither Skeleton・Strider）、End 系（Enderman テレポート・Shulker 浮遊弾・Ender Dragon 柱/第2形態）、水棲/空/ペット（Drowned・Guardian・Phantom・Wolf/Cat テイム・Horse 騎乗・Bee/Axolotl）、大型ボス（Wither・Warden 振動/Sonic Boom）および乗物（Boat / Minecart 物理拡張）をグループ単位で接続し、それぞれ 1.21.1 実クラスと挙動・ドロップを照合する。
8. **GP-7（進捗 Advancements・スコアボード・ワールドボーダー・残存コマンド）**:
   - `/fill`・`/clone`・`/execute`・`/scoreboard`・`/advancement`・`/worldborder`・`/locate`・`/spreadplayers`・`/damage` 等の未実装コマンド群、ワールドボーダー縮小・ダメージ、および進捗トリガ配信（`0x74` Update Advancements）を完成させる。Wave 5 で `/execute` の全 subcommand AST/parser（`35a075f`）・parsed pipeline 評価（`f6e2d98`）・wire expectation（`da6e5c0`; #180〜#189）を完了。`execute in ... if block` の dimension-scoped readback は実装済み（`execute_condition` の `Block` 分岐が `block_at_dim(source.dimension,…)` を読み、`In` が `coordinate_scale` を適用する。`execute_in_nether_and_end_if_block_reads_dimension_state` で固定）。

**出口:** M2（遊べる基本セッション）から M4（準拠：全 130 エンティティ・レッドストーン回路・村人取引・高度プレイヤー機構・主要コマンド）までの機能ギャップがすべて R7 配下の検証付き実装で埋まり、同一入力の Vanilla 1.21.1 対照で一致を確認できる。

## Alpha 5 — 1.21.1 ビジュアル・Mod 互換・監査収束 & 直感 Diff UX

`alpha5/integration` にて、バニラ 1.21.1 のビジュアルパリティ（一人称ビューモデル・破壊クラック・360°パノラマ・公式テクスチャ空/天候/オーバーレイ・実ベイクモデル・エンティティ外見アトラス・3Dアイコン）、Codex 独立監査（Audit 1〜5＋脱ハードコード監査）の P0/P1 閉鎖、および実用 Mod 運用基盤を3波構成で収束させる：

1. **Wave 1〜2（`alpha5/integration` へ統合済）**:
   - クライアント実描画統合（`blocks_1211.json` 由来カノニカル状態名・ベイク済地形 GPU 描画・`window.rs` 公式エクストラ配線）および Audit 2 アセット/UV/フォント/スキン修正。
   - サーバー Audit 1 修正（P0 襲撃セーブ再起動不能バグ・原子的マルチファイル世代スナップショット＋親ディレクトリ `fsync`・Anvil シード先読み・`.mcc`/`entities/*.mca`・JAR 検証＆Protocol 767 構成交換・PortalForcer・ベッド爆発/再検証・エンドラ戦＆4クリスタル再召喚・ウィザー召喚・ウォーデン振動化・構造物ルート重み/ロール数/単一 RNG・特殊レシピ＆クラフター・村人取引コンポーネント・昼洞窟湧き）。
   - Mod ワイヤ＆依存基盤（P0 `SW6D`/`SHLM` 共有コーデック・P0 `MRS1` v2 デコーダ・キネティック次元分離＆分割・汎用 NeoForge/Fabric/Quilt/Jar-in-Jar 依存ソルバ＆Modrinth 推移取得）。
   - **PLaMo 3 Translate オンデマンド Mod 日本語自動翻訳バックエンド＆UI**（翻訳時のみローカル `llama-server` を一時起動し終了即破棄、0.8B/2B推奨/8B プリセット、PLaMo コミュニティライセンス明記＋同意後 HF 取得、プレースホルダ保護、`resourcepacks/megu_auto_ja_jp/` 標準パック出力＆即時 ON/OFF トグル）。
   - **商標衛生中立化＋自動スキャンゲート（T1）**、**不足依存自動取得＋ウィンドウ維持インプロセス再起動＋Diff トースト＆1クリック巻戻し（W1A/W1B/W1C）**、**Create / Sable / Aeronautics サーバー権限＋Mod ルートチェスト/GLM ライブ配線＋Provenance Diff（M3A/M3B/M3C）**。
2. **Wave 3（収束・最終検証・出口）**:
   - **All-UI 監査（Audit 4）＋6幕サバイバル E2E 監査（Audit 5）の全 P0/P1 修正レーン**（バニラ 1.21.1 味 UI・内蔵モジュール切替・ネイティブ 20Hz サバイバル物理・本/看板/エンドクレジット/チャット P0・ボス/トライアル/ウォーデン実配線）および脱ハードコード監査の最優先バグ **A1**（Create＋汎用 Mod 混載 `MRS1` の全件反復インストール）を統合。
   - 同一 SHA での全クレート clippy/test・`perf_antipattern_audit`・`general_defect_audit --strict`・`playblocker_guard_audit`・`trademark_hygiene_audit --strict`・`docs_sync_audit --strict`・`--dst-check`・`alpha_release_smoke`（7/7）・`live_playability_e2e`（5/5）・`export_public_branch.sh --verify` を通過させる。

## Alpha 6 — 脱ハードコード基盤・Diff 駆動ワールド＆超省 VRAM 描画アーキテクチャ（次期計画）

Alpha 5 の安定化と回帰ゲート確立後に着手する中核拡張：
- **冒頭ウェーブ · 大規模脱ハードコード・単一生成・分岐削減（[DESIGN.md](../DESIGN.md) Tier A2〜A8 → Tier B1〜B6）**: Alpha 5 の回帰ゲートを安全網に、1,333 アイテム／57 コンポーネント三重重複の単一生成（A2）、スロット／姿勢／HUD 幾何の型・数式統一（A3〜A5）、染料・境界・XP 閉形式（A6〜A8）、クライアント／サーバー共有採掘＆物理核（B1）、1.21.1／26.3 バージョン別ゲームプレイ事実パック抽出（B2）、`MRS1`/`MSY1` 単一ワイヤー契約＆`WorkstationSpec`（B3）、`DirBasis`＋GPU ビットスキーマ生成（B4）、Worldgen 命令融合（B5）、Aeronautics 材質キャッシュ（B6）と [architecture-crates.md](../docs/architecture-crates.md) のドメイン分離を連動させてコード量・分岐・ドリフトを大幅圧縮する。
- **決定論 Delta ワールド保存＆タイムトラベルスナップショット**: シードから決定論再現できる未変更チャンクを 0 バイト化し、変更チャンクのみベース地形からのブロック差分（Delta）＋世代マニフェストで保存。ワールド容量を劇減させ、Mod 導入前／ボス戦前／建築ミス前への高速ロールバック・分岐試遊を実現する。
- **マルチプレイヤー チャンクハッシュ差分同期**: クライアントのローカルチャンク Merkle ハッシュを照合し、再接続・再訪・ポータル移動時に変更ブロック差分だけ送信して帯域と待ち時間を削減する。
- **ワールド内ゴースト Diff プレビュー＆1クリック Undo**: Create / Sable の船体組立てやクリエイティブ範囲編集の確定前に変更ブロックを半透明ゴーストで重ね表示し、確認・取り消しを直感化する。
- **S1. 面単位プル描画（Vertex Pulling：48 B/面 → 20〜24 B/面）**: 地形クワッドの 4 頂点展開を廃し、面レコード（始点・幅・高さ・面属性・4 隅 AO/光源）から GPU 上で 4 隅を数式復元することで VRAM と転送量を最大約 50% 削減する（`RenderRegion` 間接描画・影パス・外部シェーダーパック互換層＋独立ベンチ付きで段階導入）。
- **S2. GPU Residency Manager（安全退避＋オンデマンド再ベイク）**: `evidence/mesh-residency-status.md` の未解決課題を解消し、`alive_but_evicted` 状態と常駐 `PalettedSection` からのオンデマンド再生成によって長描画距離・内蔵 GPU でも VRAM 使用量を予算内に抑える。
- **A1. 実 WorldGen＋変更差分ベースの遠距離 LOD**: `FarHeightBiomeSampler` をテスト用ノイズから実サーバー決定論 WorldGen＋変更済みチャンク表面差分へ接続し、本物の地形と拠点を遠景へ反映する。
- **S3. ブロックモデル形状テンプレートと材質の分離**: 階段・フェンス・ハーフ等の同一ジオメトリを状態間で共有し、テクスチャ/Tint のみ材質側へ分離してベイク負荷とキャッシュ RAM を削減する。
- **A2. コールドチャンク／保存／転送限定の数理圧縮**: ホットパスのランダムアクセス性能を落とさないようライブチャンクは現行 `PalettedSection` を維持し、コールドチャンク・ディスク保存・ネットワーク同期に限定して Delta＋RLE／同一セクション共有を実測ベンチ付きで適用する。
- **`.mplugin` / `.mmod` サーバー WASM プラグイン基盤 & 定番 7 本柱モジュール群**: Mohist 型の重い JVM 密着結合は採らず、既存 `wasm-mod-runtime` に権限交差・ステージング反映・コマンド/イベント/KV API・無停止ホットリロードと作成ガイド（`docs/plugin-author-guide.md`＋テンプレート）を整備する。あわせて需要の高い定番サーバー機能 7 本柱（① コンテナ・扉の単体保護、② 既存 `claims.rs` を拡張した町・国家・外交・プロット経済、③ Delta 基盤共有のブロック/チェスト変更履歴＆局所ロールバック監査、④ 権限グループ＆ロール管理、⑤ `/home`・`/tpa`・`/warp`・軽量経済＆チェスト商店、⑥ 領土ポリゴン対応リアルタイム Web マップ、⑦ `PalettedSection` 直接バッチ更新＋ゴースト Diff プレビュー＆世代 Undo/Redo・`.schem` 対応の超高速範囲建築＆地形エディタ）を `megu-modules.toml` 全 OFF 既定（純バニラ起動）・ON/OFF 可能な軽量・商標中立の公式モジュール群として提供する。また定番 Mod の CLI 導入は起動時通信ゼロ・完全オプトイン＋SHA-512 検証＋`megu-mods.lock.json` ロック再現（`MEGU_DISABLE_MOD_DOWNLOAD=1` で完全遮断可）とし、OP/コンソール向けにカウントダウン・原子的保存・supervisor/自己 `exec` 連携の **`/restart [秒数] [理由]` / `/restart cancel`** コマンドも備える。
- **汎用 Mod 互換エコシステム拡張（4 層ピラミッド・自動互換マトリクス・Modpack 1 クリック導入）**: [docs/mod-compatibility.md](../docs/mod-compatibility.md) のローダー／バージョン非依存 4 層構造（Layer 1 静的バイトコード属性抽出 → Layer 2 統一 `WorldTxn`＋4 大 Capability 共通バス → Layer 3 `bytecode_subset_vm` 最小 Facade → Layer 4 隔離サイドカー JVM＋2D/3D 描画ブリッジ）を段階実装し、人気 Mod 上位 100〜500 件の自動互換マトリクス CI（完全動作／一部制限／未対応の自動判定表）、`.mrpack` / CurseForge Modpack の 1 クリック導入＋内蔵機能と重複する性能 Mod の自動スキップ提案、およびバージョン指定不要の `.mmod` / `.mplugin` 低ハードル SDK（WASM＋ホットリロード＋テンプレート）を整備する。
- **分散アーケードモード & ゼロトラスト MCChat インスタンス基盤（Alpha 6+ / Post-Alpha ビジョン）**:
  - **2 段構えのオプトイン・アーケード UI（純バニラ導線を邪魔せず `Modとパック` / マルチロビーから起動）**:
    - **上段 · クラシック・コンソール風アーケード枠**: 投票ロビー→中央チェスト乱戦＆補充＆脱落観戦（Battle 系）、雪玉／シャベルによる多層足場崩し（Tumble 系）、リング＆上昇気流のエリトラ滑空タイムアタック（Glide 系）の 3 種ルールエンジン。
    - **別枠 · パーティ＆クラシックミニゲーム集**: ブロックかくれんぼ（Hide & Seek 系）、感染サバイバル／ゾンビ逃走、TNT ラン（歩いた足場が消える）、雪玉ノックバック落とし等を同じ汎用アーケードルールエンジン（宣言データ＋燃料制限 WASM）上で動かせる拡張枠。
  - **クリーンルーム厳守の「エンジンとマップの分離」**: Megu 本体にはルールエンジンと商標・著作権クリーンなオリジナル同梱マップのみを収め、公式マップデータ・BGM・商標は一切同梱しない。一方で標準データパック形式（`.schem` / `.megu-arcade`）を採用し、プレイヤー自身やコミュニティが用意した再現系マップを `arcade-maps/` へ置くだけで同等のルール・手触りで遊べる設計にする。
  - **ほぼサーバーレスのパブリックマッチング＆VRChat 風分散ソーシャル（MCChat）**:
    - 既存 `FRL3` E2EE リレー・`VCR1` 近接ボイス・`ANM1` アニメーション・チャンクハッシュ差分同期＋ソフトリロードを統合し、`Public / Friends+ / Invite Only` のインスタンスと、超軽量エッジ（CF Workers + Durable Objects 等）による待機キュー／シグナリング／パブリックマッチングを提供。
    - **ゼロトラスト安全境界**: 信頼済みフレンド間のみ P2P 直結（遅延最小・コスト 0）、不特定多数の Public マッチは **短命署名トークン付きブラインド E2EE リレー経由** にして実 IP を相互非公開化。リレーは Megu フレーム固定・帯域/サイズ上限・増幅比 ≤ 1 で踏み台化を防止。クライアントはコード実行ゼロ（データ駆動＋燃料制限 WASM）＋`FrameBudgetGovernor` による頂点/テクスチャ/パーティクル/音声ハード上限＋ワンボタンセーフモードでクラッシャーを封じ込める。ゲーム本体は決定論ロックステップ＋Commit-Reveal 乱数＋過半数 `final_state_hash` 照合による不正検知とラウンド間 Host Migration で動かし、必要時は同一プロトコルの信頼済み専用ホストへ切替可能なハイブリッド構成とする。また入室時は `mod_profile_tx`＋インプロセスソフトリロードにより通常サバイバル Mod 環境を壊さず公平プロファイルへ自動切替し、退出時に元環境へ自動復帰する。

## 横断規律

- [最小の因果差分から進める手順](worldgen-parity/investigation-workflow.md)を使う。既存captureの再利用→stage/writer特定→小さい実クラス対照→本番到達確認→compiling mutant→変更が固まった対象SHAの全gateと進める。原因の変わらない重い比較を繰り返さない。
- 比較前に、何が実行され、どのstageにあり、何がskipされたかを確かめる。値が変わらないだけで負の結果としない。
- source・固定具・実測binary・参照jarを結び付け、試験前後の正常系と変異後の復元を確認する。
- 古いcaptureの再記録を修正の代用にせず、意味の変化を説明してから更新する。
- 実装、部分parity、全体合格、性能改善、push、統合を別々に報告する。数値と履歴は証拠ページへ集約する。
