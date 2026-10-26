# ロードマップ

現在の状態は[README](README.md)、目標と合格条件は[PLAN](PLAN.md)、測定値と実行条件は各 `evidence/` を正本とします。この文書は**次に進む順序・依存関係・出口条件**を定めます。部品の実装、実クラスとの部分一致、全体合格を混同しません。

## いまの判断 — 広げる前に収束する

Horizon / Valence から使える部品は既存 owner に取り込みました。ローカルsuiteは[surface context slice](../evidence/foundation/surface-context/README.md)で **239 passed / 0 failed / 1 ignored** まで戻りました。steepのheightmap、条件式4,037行、合成stone/water列の実`buildSurface` scan 768列を実クラスと照合しました。R1のforest `(-64,32)` 3x3 same-stage比較では、**対象chunkのterrain・surface・carverは0差**、残差はfeatures 550とstructures 304に分離できました。[forest R1の証拠](../evidence/gameplay/forest-r1/README.md)を追加しました。ただしreal-noise全surface・他座標・spawn_costs適用は未確認です。新しいframework・ECS・別の衝突エンジンを追加する段階ではありません。

**次の最小slice:** forest `(-64,32)` のfeature残差を、geode（smooth_basalt/calcite）、monster room（cobblestone/mossy）、ore（lapis/diamond/coal/redstone）、tree（leaves）へ分離する。各featureを単独で発火させる正の固定具で比較し、構造物は別契約として扱う。terrain/surface/carverが対象chunkで0差であることは[forest R1の証拠](../evidence/gameplay/forest-r1/README.md)、surface steep/条件式/合成buildSurface scanは[surface contextの証拠](../evidence/foundation/surface-context/README.md)に記録した。`spawn_costs`のenergy budget適用とNaturalSpawner全体は未実装であり、全chunkの再生成や別のdonor機能へ先に広げません。

## R0 — 自前データの意味論と回帰の基準を閉じる

1. 全体テストの一次失敗とmutex poisonの連鎖を分ける。一次失敗は必要なら新しいプロセスで再現し、入力・loader・評価器・古いcaptureのどこに属するかを特定する。poisonを握り潰して緑にしない。
2. 残るdensity function、real-noise地形へのsurface適用（steep/条件式/合成scanは[照合済み](../evidence/foundation/surface-context/README.md)）、`spawn_costs`の適用を、固定1.21.1の実際の意味で評価する。aquiferのpreliminary surface・流体状態は[実クラス対照](../evidence/foundation/aquifer/README.md)、biome `carvers` のAIR listとown-jar configured_feature pathは[carverの証拠](../evidence/foundation/biome-carvers/README.md)、`spawners`の読み込みと夜間hostile選択は[biome spawnerの証拠](../evidence/foundation/biome-spawners/README.md)で限定範囲を閉じた。全seed/chunk、deep-dark実chunk、NaturalSpawner、spawn_costsのenergy budgetは未検証である。各差分を独立した小さい実クラス対照と本番経路のゲートにする。
3. surfaceはsourceのdecodeや温度条件、合成列のscanだけで閉じず、実際のbiome選択・実noise列・surface contextを含む適用結果を検査する。未知ノード・解決できない参照・落とした入力名は明示し、成功したように読み替えない。異なるseedや生成器の寿命をまたぐnoise cacheと、循環参照のplaceholderも、比較を隠さない条件で検査する。

**出口:** 比較対象のrouter/biome/surface経路に未説明のゼロ代替・参照落ちが残らず、未解決・未対応の一覧と本番での到達性を説明できる。ローカルsuiteの緑は必要条件であり、whole-world・native対照・CIの代用にしない。意味を変えた経路には実クラス対照と故障を検出するテストがある。全体の赤は一次原因へ帰属させ、原因未確認のcapture更新で消さない。個別の修正ごとに広い回帰を確認し、全体合格は必須テストが通ってから判定する。

根拠: [自前データ監査](../evidence/foundation/audit-snapshots/README.md)、[sampler/native noiseの検証範囲](../evidence/foundation/weird-sampler/README.md)、[aquiferの検証範囲](../evidence/foundation/aquifer/README.md)、[carverの検証範囲](../evidence/foundation/biome-carvers/README.md)、[biome spawnerの検証範囲](../evidence/foundation/biome-spawners/README.md)、[surface contextの検証範囲](../evidence/foundation/surface-context/README.md)、[surface loaderの検証範囲](../evidence/foundation/surface-loader/README.md)。

## R1 — 同じ生成stageでworldgenを再受け入れする

1. loader/evaluatorの修正後、元の座標・seedのwhole-chunk、heightmap、ore、kelp、geodeを再比較する。forest `(-64,32)` の3x3ではterrain/surface/carverが対象chunkで0差、features 550・structures 304へ分離済み（[forest R1](../evidence/gameplay/forest-r1/README.md)）。古い一時worldが無ければ`tools/chunk_world_fixture.sh`で固定jarから再構成し、参照chunkの完成と正のcoverageを先に検査する。
2. forestの残差を密度・aquifer・surface・carver・feature・生成後tickに分ける。対象chunkはすでにterrain/surface/carverが0差で、features/structuresが残る。周辺chunkのdirt/stone/grass 207差は別途列単位で帰属する。surfaceを読めるようにしたことだけで、既存の大きな残差が消えたとは扱わない。
3. biome lookup、隣接chunkの書込み、decorating order、乱数消費を、同じstageの参照と照合する。Xoroshiro `nextInt` の32bit unsignedしきい値とbiome treeのtie順は[実クラスvectorで検証済み](../evidence/foundation/upstream-prs/README.md)であり、同じstageの比較で使う。kelpの列やgeodeのノイズなど、残差を最小の中間値まで戻して検査する。
4. 未検証featureは到達する正の固定具を用意してから広げる。未実装のfossilにはtemplate/processor、sculk patchにはspreader/runtimeの契約が先に必要。構造物やjigsawまで対応したと一括で主張しない。

**出口:** 宣言した範囲で、全参照chunkの完成・対象経路の実行・セル/状態/heightmapの比較が成立し、差分集合・除外・skipを説明できる。両側ゼロ、全skip、保存worldのruntime差をfeature差と呼ぶ比較は合格にしない。未実装範囲は別に残す。

根拠: [比較の約束](../DESIGN.md#比較の約束)、[worldgenの比較記録](../evidence/gameplay/carvers/README.md)、[forest R1](../evidence/gameplay/forest-r1/README.md)、[featureの部分検証](../evidence/gameplay/feature-parity/README.md)、[surface contextの検証範囲](../evidence/foundation/surface-context/README.md)。

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
