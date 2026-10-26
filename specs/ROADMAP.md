# ロードマップ

現在の状態は[README](README.md)、目標と合格条件は[PLAN](PLAN.md)、測定値と実行条件は各 `evidence/` を正本とします。この文書は**次に進む順序・依存関係・出口条件**を定めます。部品の実装、実クラスとの部分一致、全体合格を混同しません。

## いまの判断 — R1のfeature層を、fixture契約から閉じる

現在の checkpoint は **M2未達**です。ローカルsuiteは **247 passed / 0 failed / 1 ignored** ですが、これは全体合格ではありません。R0の自前データ意味論は限定範囲で閉じ、R1のforest `(-64,32)` では次まで到達しています。

| 比較 | 結果 | 意味 |
|---|---:|---|
| target chunk の terrain / surface / carver | **0差** | 対象chunkの基盤は同じstageで一致 |
| tree one-feature inner 3x3（`--tick`なし） | **0差** | tree origin / trunk / dirt / fancy中心枝は一致 |
| tree one-feature の `--tick`差 | 51 | saved-worldのleaf distance tick。worldgen差ではない |
| full feature 3x3（3x3 reference） | **1,680差（no-tick）** | `--tick`は保存worldのleaf/lava runtime差を加える。targetは114（`--tick`）／53（no-tick） |
| target full feature（no-tick） | **53差** | lava flow 20、ore約30、chest 1、grass 1 |
| target ore origins | **241/241一致** | origin/sizeは一致。形状または上書きが残る |
| structures | 未実装 | jigsaw/template/NBT配置は別契約 |

重要な制約:

- **decoration margin依存。** 同じtargetでも3x3 referenceと5x5 referenceで差が53→155に変わる（[margin-order](../evidence/gameplay/forest-r1/margin-order.txt)）。target chunkを単独で「worldgen残差」と呼ばず、referenceと同じmarginで比較する。
- **保存worldのruntime差をworldgen差と呼ばない。** lava flow、leaf distance、水の広がりは除外し、no-tick値と両方記録する。
- **R1の「完全一致」の判定対象は、同じseed・同じdata・同じstage・同じmarginのreference/port。** originが一致してもセル集合は一致とは限らない（ore 241/241 origins）。
- mainの取り込みは保留。remote mainの#22は、このbranchが読むcommitted datapack/assetsを削除するdata migrationであり、tree/worldgen ownerのrenameと同時に自動mergeしない。
- hosted CI #26/#27/#28はbillingでjobが起動していない。ローカル実行とhosted CIを混同しない。

**次の最小slice:** target no-tick 53のうち、runtimeでない **ore形状/上書き**を1つの共有originでセル単位に閉じる。次にchest/grass、最後に3x3全体の非target残差を同じfixture契約で分離する。構造物はその後に独立した契約として着手する。

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
3. `--tick` と `--tick`なしを両方保存する。leaf distance/lava flow/水広がりはruntime artifactとして分類し、worldgenの差分に混ぜない。
4. malformed/empty/truncated dump、wrong jar、stale provenanceは比較前に拒否する。

**出口:** 同じfixture定義を一つの文書に固定し、3x3 canonical referenceとportのtarget差分が再現する。marginを変えた比較は別fixtureとして記録し、前の数値と混ぜない。

### R1.1 tree / surface / carver（このcheckpointで閉じた範囲）

- targetのterrain/surface/carver: 0差
- tree one-feature: origin 41/41、target worldgen 0差
- `min_clipped_height`なしのclip拒否、`dirt_provider`、fancy centre limb `h2 * 0.5`、fancy attachment開始位置を実クラスbytecodeと照合
- surface materialはactual pre-surface stateを分類し、`wtop - 4` windowだけに適用

**出口:** tree one-featureのno-tick 0差を維持し、理由のないcapture更新をしない。

### R1.2 target feature残差（現在の主線）

現在のtarget no-tick 53を、次の順で1層ずつ閉じる。

1. **ore形状/上書き（約30）**
   - ore origins 241/241一致を前提にする。
   - 1つの共有originについて、referenceの `OreFeature.doPlace` とportの `gen_ore` / `gen_vein` のセル集合を比較する。
   - `OreFeature` の `BitSet` は自動拡張、portの固定Vecは境界セルを落とし得る。サイズ差は実クラスbytecodeで確認し、fixtureで効果を測る。
   - 後続featureによる上書き順（monster room/lake/spring/ore間）を同じoriginで比較する。
   - 失敗時はorigin一致を「形状一致」と読み替えない。
2. **chest 1**
   - monster roomのchest向き・位置・loot/block entityの有無を、`MonsterRoomFeature`のbytecodeと照合する。
   - chest blockだけでなく、後続のblock entity consumerが要求する範囲を分ける。
3. **grass 1**
   - patch/grassの配置predicateとbiome filterを単独fixtureで照合する。
4. **lava flow 20**
   - `SpringFeature` sourceは一致済み。flowはsaved-world tickとして分離し、worldgenの残差から除外する。
5. **非target残差**
   - targetが0になってから、3x3全体を同じfixtureで比較し、chunk境界のfeature origin/spillとruntime差を分ける。

**出口:** targetのno-tick差が0。3x3の残差は、各セルが「未実装feature」「runtime artifact」「fixture margin」「未検証」のどれかに説明され、両側ゼロ・全skip・失敗の握り潰しがない。

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

**出口:** PLANのM3条件を代表負荷で満たし、再現手順・ノイズ・未対応機能を記録する。機能不足で仕事量が減った結果や、地形が異なる条件の旧測定を性能改善へ読み替えない。

## R4 — 対象revisionの統合・リリース判定

過去の統合線の検証を、今回の移行ブランチへ引き継ぎません。作業ブランチへのpushは保存・共有であり、mainへの統合や合格とは別です。

**出口:** PLANの合格条件と公開対象の機能範囲を満たしたうえで、統合候補そのものに対して、全体テスト、必要な並列回帰・wire・adapter・層/負債ゲート、変異テスト、独立レビューを完了し、ライセンスと既知の制約を確認する。未実行・未開始のCIを緑とせず、ローカル実行とhosted CIを区別する。main更新やreleaseは、その判定とユーザーの指示を経て行う。

## R5 — 要求されたMod単位の互換拡張

現在の契約を保ち、対象Mod・版・API範囲を固定して、ロード・名前変換・イベント・状態変更の実経路を検査します。基盤全体の作り直しや汎用互換の宣言を先行させません。

**出口:** 対象の組み合わせで実行テストが成立し、その組み合わせに限った互換性を証明できる。

## 横断規律

- まず小さい実クラス対照、本番経路、故障を検出するゲートを作り、必要な節目で広い回帰へ戻る。原因の変わらない重い比較を繰り返さない。
- 比較前に、何が実行され、どのstageにあり、何がskipされたかを確かめる。値が変わらないだけで負の結果としない。
- source・固定具・実測binary・参照jarを結び付け、試験前後の正常系と変異後の復元を確認する。
- 古いcaptureの再記録を修正の代用にせず、意味の変化を説明してから更新する。
- 実装、部分parity、全体合格、性能改善、push、統合を別々に報告する。数値と履歴は証拠ページへ集約する。
