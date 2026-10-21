# 由来（provenance）監査: 「設計だけ参照」の実測

2026-10-21。#3（由来メモ）は「Pumpkin など他実装の GPL コードは取り込んでいない」
と宣言しているが、`src/tree.rs` の冒頭に "Ported from pumpkin … semantics" と
書かれていたので、宣言が事実かを測った。宣言そのものは変えていない。判断材料を残す。

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

つまり「同じアルゴリズムを独立に書いた」ときの一致は **16〜21 トークン**まで。

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
リング条件で同じ性質。コメント一致が 0 であることと合わせて、
**コピーではなく同じ仕様の独立実装**と判断する（Jaccard も対照実験の範囲内）。

## 他の「pumpkin」への言及（同じ目で見た結果）

| 箇所 | 内容 | 判断 |
|---|---|---|
| `src/tick_queue.rs` 冒頭 | 設計のみ参照、GPL なので独立実装と明記 | 問題なし（既に明記） |
| `src/block_registry.rs` の `state_flags`/`side_flags` コメント | データ項目の意味の出所 | 問題なし（データは Mojang 由来） |
| `src/generation_math.rs` の xoroshiro テストベクタ | 数値（事実） | 問題なし |
| `src/tree.rs` 冒頭 | "Ported from pumpkin … semantics" | 実測は独立実装。文言を「semantics を参照」に寄せ、本監査を指す |

## この監査が覆る条件

- `10 *` 以上のファイルで最長一致が 100 トークンを超える
- 一致コメント行が 1 行でも出る
- Pumpkin のコードを貼った後に「変数名だけ変える」編集をした場合（Jaccard は
  下がるが最長一致は残るので、上の 2 指標で捕まる）

再実行は `tools/provenance_scan.py` と、Pumpkin の該当ディレクトリだけで足りる。
