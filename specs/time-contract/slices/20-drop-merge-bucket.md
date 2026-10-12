# 20 — dropマージのセルバケット化

## なぜ

fixture 19で発見：実体を含めるとtick 667ms、その72%がdrops。
原因は `drop_tick` のマージが全ペア比較のO(n²)。

## 契約

- セルバケットで近傍27セルのみ比較。
- `i` は元の順序、候補は `j > i` を**元index昇順**で試す、最初の一致でbreak。
- 座標の隣接計算は `checked_add`。飽和した極端座標では隣が存在しない。
  wrappingはセルキーのエイリアスを招き、誤マージの原因になる。
- 条件は安い判定（item一致・距離）を先、`comps` のVec比較を後。

## ゲート

`bucketed_merge_absorbs_exactly_what_the_full_pairwise_scan_absorbs` が
**元の全ペアscanをそのまま書いた参照実装**と9シナリオで一致することを検証。
境界またぎ（x/yの整数境をまたぐペア）、混在item/comps、スタック上限、
年齢による保持選択、遠方非マージ、連鎖、決定論的散布を含む。

`bucketed_merge_still_merges_at_all` がバケットが何もマージしない
（見かけ上通る）退化を防ぐ。

## 等価ではない案

sortしてから比較。吸収されるドロップが変わるので却下。

## 残る

- entity_heavy はまだ 339ms/tick。drops stage が 149ms で支配的。
  600ドロップが狭い範囲に集中すると近傍数も増える。分散負荷が対象。
- `environment` が 27%。未調査。
