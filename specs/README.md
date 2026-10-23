# 現在状態

このページは、現在の作業状態と次の判断の入口です。ブランチ名・HEAD・コミット数のように短期間で変わる値は記録しません。測定値と再現条件の詳細は各証拠ページを正本とし、ここでは結論と限界だけを要約します。

## 文書の役割

| 質問 | 正本 |
|---|---|
| いま何ができ、何が未確認か | このページ |
| 目標・合格条件 | [PLAN.md](PLAN.md) |
| 次に進む順序と各段階の出口条件 | [ROADMAP.md](ROADMAP.md) |
| なぜその設計を選んだか | [choices](close-defects/choices.md) と各spec |
| 数値・fixture・再現条件 | 該当する [`evidence/`](../evidence/) ページ |
| 過去の個別監査項目 | [close-defects](close-defects/README.md)（現在の全体完了判定ではない） |

READMEや他の計画文書に状態表を複製しません。更新時はこのページと根拠となる証拠を一緒に直します。

## 状態

- **全体目標は未達。** M2（同じ入力で遊べる範囲のvanilla一致）とM3（同じ仕事での軽量性）は達成していません。完了条件は[PLAN](PLAN.md)を参照。
- **worldgenは進行中。** terrain-only比較と cave / cave-extra / canyon carver は限定した2×2固定具で0セル差を確認しています。これは全seed・全chunk・featureを含む一致を示しません。
- **feature層は未確定。** 以前の差分値の一部は、生成stage・参照chunkの完成度・skip数が揃っていないため、現行feature残差として扱いません。保存world全体の差をfeature差と呼ばないこと。
- **kelpは実装済みだがvanilla parityは未検証。** 合成回帰テストはありますが、既存のvanilla fixtureでは両側ともkelpが0個で、挙動を通った証拠になりません。正の参照セルを持つ完全生成chunkが必要です。
- **構造物は未実装。** jigsaw、template pool、NBT経由の構造物配置は未対応です。
- **M3は未達。** 地形が一致しない条件で得た旧x0.63値は性能根拠に使いません。比較条件・最新の測定と限界は[生成器の費用](../evidence/gameplay/worldgen-cost/README.md)を参照してください。
- **コード検証:** `cargo test --release` は `b3e1c62` のコードで157 passed / 1 ignored。今回の変更は文書のみです。この記録は `tests/verify.sh` やhosted CIの成功を意味しません。`cargo fmt -- --check` は既存の広範な差分を報告しており、一括整形はしていません。

## 次の判別作業

feature parity を進める前に、**正のkelp参照を含むvanilla fixture**を作ります。選択されたplaced feature、全生成済み参照chunk、kelp/kelp-plantの正の個数をゲートで確認し、その後にfeature実行・乱数消費・セル集合を比較します。両側0個、全chunk skip、別stageの差分は成功扱いにしません。

以後の順序は[ロードマップ](ROADMAP.md)に従います。比較規律は[DESIGN.mdの約束](../DESIGN.md#比較の約束)を参照してください。
