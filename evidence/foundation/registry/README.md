# slice 03 — content revision と smelt 解決のキャッシュ

## 何を変えたか

- `CONTENT_REVISION` を追加し、datapack読込の**唯一の出口**で上げる。
- `smelt_out(item)` から closure 引数を外し、`Option<(i64, u8)>` を返す。
- 炉の `recipe_cache` のキーを `(item, revision)` にし、**可変部を含む全体**を
  キャッシュする（以前は不変部だけ）。
- `CachedLookup` を**多エントリ（8件）**にした。

## 着手前の制約は2つとも解けた

**1. `Comps` は不要だった。** `smelt_out` は
`Some((*o, *c, Vec::new()))` を返しており、**第3要素は常に空Vec**。
呼び出し側も `o.0` / `o.1` しか使わない。よって Copy な射影で足りる。
`f.output.2 == o.2` は `f.output.2.is_empty()` に置き換えた
（smelt出力は成分を持たないので、成分を持つ出力とは合流できない）。

**2. `CachedLookup` の消費者は炉だけだった。** SeedProtect は別の型。
当初「共有コードの変更は影響確認が要る」と記録したが、確認したら共有していなかった。

## 測定：1エントリのキャッシュは**何も速くしていなかった**

400 furnace分のアイテム × datapackレシピ40件 × 1000巡（release）:

| 方式 | 時間 | 対 無キャッシュ |
|---|---|---|
| 無キャッシュ | 476 ms | — |
| **1エントリ**（旧） | 476 ms | **x1.00** |
| 8エントリ（新） | 0.41 ms | **x1158** |
| HashMap | 2.25 ms | x211 |

**旧キャッシュは5品目を巡回しただけでスラッシングし、毎回ミスしていた。**
性能測定が「利得なし」を出したので原因を追い、容量だと分かった。
8エントリの線形走査はHashMapより速い（キーが5個なので早期にヒットする）。

ゲート `cached_smelting_beats_rescanning_datapack_recipes` は
**x10超**を要求する（ノイズ下限4%より遥かに大きいので、比そのものではなく下限を固定）。

## 可変表の書き手は1つだけだった

`cached_tick_matches_v237_...` が **scenario 2 で失敗**した。
テストが `json_smelts()` / `item_tags()` を**直接書き換えており、
revisionが動いていなかった**。

本番でこれらの表を書くのは `load_datapacks` だけ（`json_smelts` へのpushは
1箇所、`item_tags` も1箇所）。よってテスト側が「reload」を宣言するよう
`pack_changed()` を各変更点に置いた。

**これはこの変更が持ち込んだリスクそのもの**である：
キャッシュを大きくすると、revisionを欠くキーは**より長く**誤ったままになる。
テストがそれを検出したので、契約として固定した。

## 負の対照

- `revision-negative.log`：`smelt_out` から可変部の走査を外すと
  `a_content_reload_invalidates_cached_smelting` が失敗
  （`left: Some((804,1)) / right: Some((42,3))`）。
  つまり**キャッシュが可変部を覆っていること**を検査している。

## まだ測れていないこと（重要）

**end-to-end の数字は無い。** `active_furnaces` fixture は `datapacks/` を
読まないため `json_smelts()` が空で、この費用が**そもそも発生しない**。
測ったのは**機構**（x1158）であって、fixtureのtickではない。
