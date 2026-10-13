# 現在状態の集約（次の判断の入口）

この文書は**加速層**であり、権威ではない。権威は各spec・CI・証拠。
下の各specへ降りて確認する。書いた時点の状態を1箇所にまとめ、
次のセッションが再構成せずに判断できるようにする。

## CURRENT OBJECTIVE

**小規模・低負荷の世界で計算量を減らし、必要な既存Modも使えるサーバーを作る。**
観測可能な挙動を保つことが前提（[DESIGN.md](../DESIGN.md)）。

順序は「基盤を先に」。20TPS時間契約は完了し、いまは基盤の共通部品を作っている。
「超軽量化の実証」は**まだ達成していない**。

## CURRENT AUTHORITY / HEAD

| | |
|---|---|
| branch | **`fix/20tps-integrated`** |
| HEAD | `2184339` |
| 状態 | ツリーclean、リモートと一致、**CI green** |
| `main` との差 | **78 commit**。`main` は `4e5f3e6`（未マージ） |
| テスト | **83 passed**（serial／8thread×3回連続）、CI 4step通過 |

`fix/20tps-integrated` = 20TPS時間契約 + PR #2のhardening の統合head。
`fix/20tps-time-contract` は時間契約のみ（`58dac5e`、統合に包含）。
`fix/audit-1-on-20tps` は削除済み。

## PROVEN / ACCEPTED

**時間契約（[time-contract](time-contract/README.md)）**
- `TICK_MS = 50`。全境界（時刻・効果・精錬・機械・移動・導火線・TTL・周期・確率）を
  `TICK_MS` から導出。独立レビュー（issue #7）の P1 は全て対応し、各修正に負の対照を付けた
- 移動係数は javap 一次証拠から vanilla 値
- 実バグを6件発見・修正（溶岩リテラル・save/load単位・air供給・TTL・座標OF・
  旧counter単位・save副作用・飛翔体drag）

**基盤（[foundation](foundation/README.md)）**
- **slice 01 Region accessor 完了**：per-item lockを1箇所に。entity-heavy fixtureで
  **x1.148 / x1.184**（測定）
- **slice 02 Scheduled-tick queue 完了**：遅延リング＋重複排除。**水・溶岩・ホッパー
  全てを移行**し、毎tick全セルclone＋巡回は**0件**。痕跡ゲート3本で等価性を確認

**重複の解消（[survey](foundation/slices/07-redundancy-survey.md)）**
- `requeue_water`/`requeue_hopper`/`requeue_lava` の3重化 → `requeue_fluid(queue,pos,period)`
- `clients()` スナップショット式 **35箇所** → `client_snapshot()`（ロック順序を関数にした）
- 4つのbroadcast → `fanout(述語, mk)`（broadcast呼び出しは94箇所）
- tree の置換可能述語3つ → `is_replaceable(reg, cur, tags)`
- 各統合にゲートと**負の対照**を付けた。うち2件は**統合した瞬間に検証の穴を発見**
  （溶岩のperiod変換、broadcastの述語）

**ブロック知識の表を資産由来にする（[slice 03](foundation/slices/03-registry.md)、[一掃](foundation/slices/08-asset-sweep.md)）**
- 手書きの表と資産を全状態で比較し、**移行前のgate**を作った（移行の等価性は
  移行前の状態を固定していなければ意味がない）
- **`block_hardness` を削除し資産から導出**：手書き表は 26,684状態中**356状態（1.3%）**
  しか覆っておらず、残りは既定0.6だった。**26,420状態の掘削時間が誤っていた**
  （`crying_obsidian` は50ではなく0.6、金属ブロック・spawner・anvil・hopper・chest・deepslate）
- **`canHarvest` を資産の `harvestTools` から導出**：旧判定は
  「ツルハシが要る かつ tierが足りる」という**別の質問**で、道具が要らないブロック
  （土・砂・草・原木・葉）を「不可」と判定し**3.33倍遅く**していた。
  `harvest_level_required` / `harvest_level`（約30行）を削除
- **`tool_dig_speed` の正しいツール種も資産から導出**：旧判定は裸の数値範囲で、
  157状態+119+19しか覆っておらず **83.9% の状態で誤っていた**（土にシャベル、
  原木に斧、苗木に斧、葉にクワ）。`needs_pickaxe` を削除。
  黒曜石のように `material` が tier しか述べない場合は `harvestTools` の
  アイテムIDからツール種が決まる
- 3件とも**資産を独立に読む**gateを持ち、負の対照で失敗を確認

**セキュリティ**：PR #2のhardeningを統合（権限・RCON・handshake・save envelope・
起動時検証・pre-play deadline）

## DISPROVEN / SUPERSEDED

**再開しないこと（negative knowledge）**
- **PR #2の素朴な20TPS（`56a6e79`）は取り込まない。** `TICK_MS` を50にするだけで
  効果持続・炉・空気・TTL・移動係数を変換しておらず、取り込むと後退する。
  pacing部分だけ移植済み
- **region構築のVec+sort+dedupは不採用**。密集した中心で x0.72
- **drop/xp/作物/mobのlock hoistは不採用**（fixtureにその要素が無く測れない）
- **`save_world_into` の旧実装はデータ損失**。resting tridentがsaveから失われていた

**重複の統合で実際に起きたこと（記録）**
- 統合は**検証の穴を露出させる**。溶岩のperiod変換は痕跡ゲートをすり抜けていた
  （流れるセルはbackoffせず、periodが効かない分岐しか通らない）。
  broadcastの述語も同様。**「重複している」と「ゲートが覆っている」は別**
- クローン検出の J は**候補を挙げるだけ**。`lerp2`/`lerp3` は J=1.00 だが
  アリティが違うので統合しない

**測定方法の失敗（繰り返さない）**
- before/afterを数分離して測ると**環境負荷を変更の効果と誤読**する。
  同一コードの再測定で壁時計8.5%・CPU時間19%変動。**連続して測る**
- **fixtureに無い要素の最適化は測れない**（slice 18で3回踏んだ）
- `tc qdisc` 等でリソース制限すると、制限が全体にかかり測定環境を壊す

## UNRESOLVED

| 項目 | 場所 |
|---|---|
| **独立レビュー**：統合headの衝突解決10箇所は未確認 | issue #7 |
| 重複の上位候補（表引き4つ組、`spawn_*_pkt`、`door_base`/`gate_base`） | [survey](foundation/slices/07-redundancy-survey.md) |
| **資産の残りフィールド**（`stackSize`, `resistance`, `luminance`, `blast_resistance`, `slipperiness`, **`collision_shapes`**） | [一掃](foundation/slices/08-asset-sweep.md) |
| **`tier_speed` の表**（木2/石4/金12/鉄6/ダイヤ8/ネト9）が手書き（資産の `harvestTools` にツール速度は無いので出典が要る） | [slice 03](foundation/slices/03-registry.md) |
| `state_to_item`（drops）も資産の `drops` を再導出している（シルクタッチ/幸運の分岐があり単純置換不可） | [slice 03](foundation/slices/03-registry.md) |
| `is_hot` / `is_warm` の薄いラッパが未使用のまま残る（警告2件） | [slice 01](foundation/slices/01-region-accessor.md) |
| 基盤 slice 03〜06 | [foundation](foundation/README.md) |
| 被ダメ無敵をvanilla `invulnerableTime` 20へ（未実装） | [slice 25](time-contract/slices/25-invulnerability.md) |
| 実クライアント・vanilla動的比較・代表負荷計測 | 全て未実施 |
| `sched_ready` が3ループに冗長に残る（掃除候補） | [tick-queue](../evidence/foundation/tick-queue/README.md) |

## INVARIANTS / DO-NOT-CHANGE

- ゲーム時間・乱数・状態はRustに一つだけ。Java側に別の世界を進めない
- 省略は結果と発生tickが不変と証明した対象だけ。**定数の時間換算を状態再現と呼ばない**
- scheduled tickの遅延値を全セルの永久巡回周期に読み替えない（slice 02で解消済み）
- 休止には漏れない起床条件が要る。**起床条件漏れは不合格**
- 時間語の変換は**周期は割る・確率は掛ける**。方向を間違えると5倍/8倍ずれる
- 永続化の単位はMET4。旧MET2/MET3はミリ秒（age系）と4TPS tick（counter系）の2種類
- 測定は **x1.10未満を主張しない**（ノイズが大きい）
- `main` に直接入れない。段階ごとにbranchを切りCIを通す

## CURRENT FIRST BLOCKER

**独立レビューが一度も通っていない。** `codex review` は外部の利用制限で不通。
統合headは衝突解決を10箇所で行い、**その正しさは未確認**。
自己レビューで私が作った欠陥を2件見つけた（`read_comps` の重複、saveの二重書込）
ので、他にも残っている可能性がある。

## FINITE DECISION TREE

**順序を入れ替えた。** 直近の作業は全て vanilla 忠実性であり、目標である
「超軽量化」の証拠は1つも増えていない。[slice 08](foundation/slices/08-asset-sweep.md)
の一掃で、忠実性の作業が**無限に続くこと**が分かった（残りは10フィールド以上）。

1. **測定器を作る（`tools/asset_coverage.py` + `tests/verify.sh`）** —
   同じ手順を4回手書きした。1コマンドにすれば、以降の全部が安くなる。
2. **スカラー系のデータフィールドを一括で移行する**
   （`stackSize`, `blast_resistance`, `slipperiness` など）— 同形の作業を
   5コミットに分けない。
3. **実環境計測の手段を作る** — vanilla比較・代表負荷・datapack入りfixture。
   **これが無い限り「超軽量化」は主張できない。**
4. **`collision_shapes`（状態ごとの衝突形状）** — 残る**最大の挙動差**。
   新しい形状系と移動・設置の消費者が要るので独立した spec にする。
5. **独立レビューを起こす** — 最大のプロセスリスク。
   `codex review` は不通なので別の経路で。
6. slice 04（Entity framework）/ 05 / 06 — 上を踏んだ後。

## NEXT SMALLEST DISCRIMINATING ACTION

**測定の手順を1つのスクリプトにする。**

「手書き表と資産を全状態で比較し、食い違いを数え、例を出す」という
**同じ手順を4回手書きした**（hardness / canHarvest / tool_kind / drops）。

`tools/asset_coverage.py --field <name>` として置けば、
残りのフィールド（`stackSize` / `resistance` / `luminance` / `blast_resistance` /
`slipperiness` / `collision_shapes`）の**それぞれが1コマンドで測れる**。

これは1ファイルで完結し、**以降の全ての決定を安くする**。

同時に `tests/verify.sh` を作る（serial → 8thread×3 → wire smoke → feature 2種）。

## EXPLICIT NON-NEXT WORK

- **vanilla coeffやTTLの再変更**（レビュー済み・確定）
- **PR #2の素朴な20TPSの取り込み**（却下済み）
- **不採用にした最適化の再試行**（測定済み）
- **fixtureに無い要素の最適化**（測れない）
- `sched_ready` の削除（安全網。急がない）

## EVIDENCE REFERENCES

- 時間契約: [specs/time-contract](time-contract/README.md) / [evidence/time-contract](../evidence/time-contract/)
- 基盤: [specs/foundation](foundation/README.md) / [evidence/foundation](../evidence/foundation/)
- Mod互換: [specs/mod-compatibility](mod-compatibility/README.md)
- 設計原則: [DESIGN.md](../DESIGN.md)
- 独立レビュー: issue #7（REQUEST CHANGES、P1は対応済み）
- Pumpkin採用の制約: issue #3（**GPL-3.0。コードは写さない**）
