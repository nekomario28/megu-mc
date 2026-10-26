# 由来（provenance）監査: 「設計だけ参照」の実測

2026-10-21 の測定の記録。#3（由来メモ）は「Pumpkin など他実装の GPL コードは取り込んでいない」
と宣言しており、当時の `src/tree.rs` の冒頭には "Ported from pumpkin … semantics" と
書かれていた。その文言と宣言がどう対応するかを測った。**この文言は後に削除済み**
（`src/tree.rs` の冒頭は、型名・feature 名が vanilla のデータパック名に従うことだけを述べる）。

> この文書は 2026-10-21 時点の測定の記録です。その後 `tree.rs` ほかのコードと名前を書き直したため、
> 現在の数値は `tools/provenance_scan.py` で再計測してください。判断（法的なものを含む）は行いません。

## 方法

`tools/provenance_scan.py` が 3 つ測る:

| 指標 | 何が分かるか | コピーのとき | 独立実装のとき |
|---|---|---|---|
| 最長の**連続**トークン一致長 | コードの逐語一致 | 数十〜数千 | 十数（ループ骨格・シグネチャ） |
| 6-gram shingle Jaccard | トークン列全体の形の重なり | 0.1〜1.0 | 0.00〜0.03 |
| 一致するコメント行 | **散文はアルゴリズムから導かれない** | 1 以上 | 0 |

コメント・文字列・文字リテラルは落としてからトークン化する（コードの形だけを見る）。
コメントは別枠で照合する。

較正は同じスクリプトで取れる:

```sh
python3 tools/provenance_scan.py /tmp/mine_carver.rs /tmp/pk_cave.rs      # 独立（同じ bytecode から別実装）
python3 tools/provenance_scan.py /tmp/mine_carver.rs /tmp/mine_carver.rs  # 同一（上限の確認）
```

## 較正点

| 比較 | 最長連続一致 | Jaccard | 備考 |
|---|---|---|---|
| 同一ファイル同士 | **6261** | 1.0000 | 上限 |
| この repo の carver ⇔ Pumpkin の `cave.rs` | 16 | 0.0219 | 同じ 1.21.1 bytecode から独立に書いた対照 |
| この repo の carver ⇔ Pumpkin の `canyon.rs` | 21 | 0.0137 | 同上 |

この 2 組（同じ 1.21.1 bytecode から別々に書いた carver）では、最長の連続一致は 16〜21 トークンだった。

## 結果: `src/tree.rs` ⇔ Pumpkin `feature/features/tree/`（38 ファイル全部）

| 指標 | 値 |
|---|---|
| 最長連続一致 | **58**（`trunk/dark_oak.rs`） |
| 最大 Jaccard | 0.0205（`trunk/fancy.rs`） |
| 一致コメント行 | **0** |

最長の 58 トークンはこれ:

```
} ) ; for ox in - 1 . . = 2 { for oz in - 1 . . = 2 {
  if ( ! ( 0 . . = 1 ) . contains ( & ox ) | | ! ( 0 . . = 1 ) . contains ( & oz ) )
```

vanilla の dark oak が 4 隅の柱を回るループで、忠実に書けば誰でもこの形になる。
2 位の 44 トークン（`decorator/alter_ground.rs`）も
`for xx in -2..=2 { for zz in -2..=2 { if xx.abs() != 2 || zz.abs() != 2` の
リング条件で同じ性質。この測定では、最長の連続一致はループの骨格で、一致コメント行は 0、
Jaccard は対照実験の範囲内だった。

## 他の「pumpkin」への言及（同じ目で見た結果）

| 箇所 | 内容 | 判断 |
|---|---|---|
| `src/tick_queue.rs` 冒頭 | 設計のみ参照と明記 | 現在も残している（設計の参照元の記録） |
| `src/block_registry.rs` の `state_flags`/`side_flags` コメント | データ項目の意味の出所 | データは Mojang 由来（現在は生成物） |
| `src/generation_math.rs` の xoroshiro テストベクタ | 他実装のテストの数値と一致 | **削除**。jar から生成した照合値（`tests/data/vanilla_vectors_1211.tsv`）に置き換えた |
| `src/tree.rs` 冒頭 | "Ported from pumpkin … semantics" | **削除**。vanilla のデータパック名に従う旨だけを書く |

## この測定の見方

- 最長一致が長い、または一致コメント行がある場合は、該当箇所を個別に確認する材料になる
- 変数名だけを変えた編集では Jaccard は下がるが最長一致は残るので、上の 2 指標を併せて見る
- 測定は目安であり、複製の有無や権利についての結論ではない

再実行は `tools/provenance_scan.py` と、Pumpkin の該当ディレクトリだけで足りる。
