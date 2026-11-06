# 実装・検証チェックリスト（`perf/worldgen-parallel`）

作成: 2026-11-06。基点: `48dfe73b4e1db64d5ce295f0e533d30780a8fc37`。
状態: **計画のみ**。この文書の追加は実装・試験の完了を意味しない。

この文書は**作業順、担当する既存owner、完了ゲートを一覧化する実行台帳**。
目標の正本は [PLAN](PLAN.md)、順序と出口条件の正本は [ROADMAP](ROADMAP.md)。
詳細な意味論は [worldgen-parity](worldgen-parity/README.md)・
[template-finalization](template-finalization/README.md)・
[chunk-parallelism](chunk-parallelism/README.md)、実測値は `evidence/` を正本とし、
ここに数値や歴史記録を複製しない。

## 判断と運用

- 対象は **Minecraft Java 1.21.1 / protocol 767**、固定公式jar `59353fb40c36d304f2035d51e7d6e6baa98dc05c`。
- **最初は計画だけのDraft PRを、この作業ブランチ宛て**に提出する。プログラム変更、`main` 更新、公開・releaseは別の判断。既存のhost上の未コミット作業（`fossil-placement-20261106` 監査の34ファイルなど）は上書き・stash・restoreしない。
- 各項目は **native実クラスの同一入力 → Rust独立比較 → 本番経路で再実行 → 故障注入（compiling mutant）→ 広い回帰** の順で閉じる。fixtureの0件実行・skip・old captureを「一致」に数えない。
- 同じseed、dimension、datapack、structures設定、source順、margin、chunk FULL、時計、生成stageを固定。**生成停止（paused）とruntime tick後を別々**に比較し、全block state、書き込み順、RNG、NBT、pending/delivered tick、保存/再読込、wireを混同しない。
- **ステータスは実際に検証した項目だけ `[x]`** にする。`tests/verify.sh`、native JAR比較、GitHub Actions、第三者レビューは別々に記録する。ホストCI未起動は成功ではない。
- 既に確認済みの限定契約（forest 3×3の旧checkpoint、template processor・fossil・proto tickのnative fixtures）は消さずに回帰として維持する。新HEADでの全体合格には置き換えない。

## 優先順・依存関係・完了条件

### P0 — 作業の検証基盤と入力意味論（並行して先行）

- [ ] **P0-1 HEAD再現と失敗の基線固定** — `48dfe73b4` のコード・入力資産・固定jarのdigestを紐付け、`tests/verify.sh`（serial / 8-thread×3 / wire / adapter / layers / debt / mutations）で現在の一次失敗を収集。テスト失敗をmutex poisonやCI課金停止と混同しない。各結果に実行環境・終了コード・省略項目を記録する。
- [ ] **P0-2 1.21.1 density/surface/biomeの到達性監査** — `src/worldgen.rs` の未知density型 `DF::Zero`、明示のゼロ扱いノード、surface ruleの未対応、`spawn_costs` とNaturalSpawner等を分類。固定datapackの実際に到達する経路を列挙し、必須の未知ノードは黙ってゼロにせず拒否する。修正は実クラスfixtureと本番チャンク差分で確認。未到達な旧形式の互換性を推測して広げない。
- [ ] **P0-3 再配布・出所の隔離判断（統合/公開ブロッカー）** — この枝は `datapacks/vanilla/`、1,180 NBTテンプレート、`config_packets.bin`、`assets/pblocks.json` 等を追跡しており、`main` の「jarからローカル生成・非コミット」方針およびREADME/THIRD_PARTYの記述と矛盾する。各blobの出所・権利・ハッシュと再生成手順を一覧化し、検証可能なローカル生成物へ切り替える方針を確定する。**無条件の一括削除でテスト入力を消さない**。LICENSE・Pumpkin一致・履歴・第三者資産を含め確認が済むまで `main` へマージ／公開しない。内部実装の他のP1作業は継続可能。
- [ ] **P0-4 native比較契約の固定** — Fossilが**正に配置される**fixture、complete FULL chunks、必要halo、sourceの生成順、2回のnative採取の再現性を定義。artifactに入力・SHA・対象セル数・除外数・pending tick・writerを記録し、古い測定値の流用を防ぐ。

### P1 — Template finalization（**最優先の実装列**）

- [x] **P1-0 完了済みの限定基礎（再回帰が必要）** — template/tree共通 `src/voxel_faces.rs` の面走査、液体予約順、`src/scheduled_ticks.rs` のchunkごとの生成tick unpack、bounded full-cube Fossilのnativeオラクル。**これらは最終state・実配送・一般構造物の合格ではない**。
- [ ] **P1-1 隣接stateの最終確定** — [仕様](template-finalization/slices/01-neighbors.md)。既存 `src/structure_template.rs` / `src/tree.rs` / runtime ownerを確認し、`updateShapeAtEdge` と `updateFromNeighbourShapes` の変更セルと順序・予約tickをnativeと比較。waterlogged・connected・fragile・falling・mixed fluid、full-cubeでもshape更新を起こす例を含む。新しい第2の状態機械を作らず単一ownerへ統合。未知呼び出しはfail closed。
- [ ] **P1-2 Block Entity / Entity初期化** — [仕様](template-finalization/slices/02-initialization.md)。NBT付きtemplate・entityを使いnativeの受理/拒否、回転、座標/UUID、loot seed、finalizerを観察。 `src/structure_template.rs` と既存runtime NBT/保存ownerを接続し、**実在する初期化済みインスタンス**とpacket・保存/別プロセス再読込を比較。既存のNBT無し許容経路は新契約が通るまで維持し、保存形式の変更はこの段で行わない。
- [ ] **P1-3 生成tickの実行・配送** — [仕様](template-finalization/slices/03-delivery.md)。protoからruntimeへの受け渡しだけでなく、native `LevelChunkTicks` / `LevelTicks` とゲーム時刻・priority・sub-order・cross-chunk・chunk eligibility・上限の**実際に実行されたイベント列と最終状態**を比較。block/fluidを独立→統合、保存/再開と未ロードchunkを含む。既存`scheduled_ticks`とGEN-lock-free drainを保つ。
- [ ] **P1-4 実チャンクFossil受け入れ** — [仕様](template-finalization/slices/04-chunks.md)。実際にFossilが置かれるnativeの完全chunkを採取し、同一stage・source順・marginで全state、heightmap、block entity、pending tickを比較。続けてruntime tickとsave/reload、light・wireは**別のgate**で評価する。scripted-level oracleの緑だけで閉じない。

### P2 — 全WorldGenへ段階的拡張

- [ ] **P2-1 汎用template / processor / structure starts** — [仕様](worldgen-parity/README.md)。現在のfossil専用full-cube制約を拡張する前に、processorの拒否理由（gravity、位置条件、NBT変更、holder等）をケース別にnative照合。Template Pool / Jigsaw / structure placement / start・reference / exclusionとbiome relocationを実装。**34定義・16アルゴリズム型**を別々に扱い、実配置・NBT・tickを持つ正のfixtureで受け入れを記録。
- [ ] **P2-2 残るconfigured featuresと実chunk差分** — `src/generation_contract_tests.rs` の明示未実装 `bonus_chest`・`end_spike` を対応。codecは外側のmatchだけでなく必須child/provider/modifierの実パースとRNG消費を検証。`sculk_patch` は旧監査の52/213差分を最新HEAD・同一population順で再計測し `sculk_gate.py --require-match` で判断。特定seedで残る `weeping_vines` 1セル差も独立fixtureで閉じる。差が増えた理由は到達率と実行stageに分解する。
- [ ] **P2-3 三dimensionの通常サーバー経路** — Overworldの多biome境界、Nether/Endのterrain・features・structures、surface、aquifer、carver、spawnsを実クラス・full chunkで比較。`MC_SPINE_WG_PRESET=end|nether`＋`NOFEAT` はoffline terrain診断に限定されるため、通常のserverのchunk生成→runtime→save/reload→protocolまで別途実装して確認。必要な代表seed・負座標・構造物境界を含める。

### P3 — M2 gameplay / M3性能（WorldGenと証拠を混ぜない）

- [ ] **P3-1 M2観測可能なゲーム操作** — [合格範囲](PLAN.md)。光・衝突shape・プレイヤー操作・inventory/chest・item pickup・block updates・mob・戦闘・死/respawn・craft/炉・天候/昼夜を、1.21.1 native・実wire・別接続での状態再取得・保存後で確認。旧gameplayの進捗表に古い数値・「未着手」が混在するため、現行コードと証拠で各項目を再棚卸しし、未実装と未検証を分離する。
- [ ] **P3-2 生成の並列化と性能gate** — [仕様](chunk-parallelism/README.md)。現在はterrain先読みworkerとtickロック分離が中心でfeatureの完全並列生成は未証明。worker 1/4/8で出力digest・RNG・実チャンクの全stateを比較し、ownerによる**vanillaと同じ書込み順**で確定。panic・キャッシュ世代・peak RSSを確認してから `WARM=1` の同一負荷CPU秒・RSS・20TPS・tick P99を測定。CPU優位を測れないならM3未達と記載。

### P4 — 統合前のゲート（自動ではmergeしない）

- [ ] **P4-1 同一SHAの全gateと独立レビュー** — `tests/verify.sh` 完走、native oracle、本番wire、mutation、serial/parallel、adapter、layers/debt、Fossil/structure/feature・dimension回帰の結果を同一revisionで収集。hosted CIを使えない場合は未実行と明記。過去の[Issue #7](https://github.com/nekomario28/mc-spine/issues/7)は古いSHAのレビューなので、現行変更に対して独立レビューを取り直す。
- [ ] **P4-2 配布物・ライセンス・リリース判定** — P0-3を完了してtracking/生成物、NOTICE、依存ライセンスと履歴を照合。M2/M3/G1〜G4の証拠と既知の未対応範囲を明示して初めて `main` 向け実装PR/リリースを判断する。合格していないテストを緑扱いしない。

## 次の着手

1. **P0-1/P0-2/P0-4** を検証基盤として固定。P0-3の出所調査は同時進行するが、内部実装を全停止させない。
2. 最初のコードPRは **P1-1（neighbor state transitions）だけ**。既存 `template-finalization` の単一owner規則に従い、native失敗fixtureとそれを直す最小実装をセットにする。
3. **P1-2 → P1-3 → P1-4** を個別の小さな変更と検証単位で進める。各段の完了はこの台帳のチェックを更新し、具体的な再現値は `evidence/` に記録。
4. **P2以降は先行ゲートを満たした部分から**展開する。P0-3/P4が未解決の間は `main` へ統合・外部配布しない。

## レビューで明示する未実行・未解決

作成時点のHEADコミット本文にはRustテスト **384 passed / 1 ignored**、
**255 mutants** が記録されている。これは当該コミットの自己申告の検証記録で、
本計画PRで独立再実行した結果ではない。hosted CIにも当該作業ブランチの実行記録はない。
現行ソースには汎用neighbor遷移、template entityの実初期化、
unrestricted runtime tick、全chunk parityの実装/受け入れが残る。
