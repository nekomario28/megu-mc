# ランダムtick と最初の消費者（サトウキビ）

`src/world_rng.rs`、`random_tick_pass`、`on_random_tick`、`sugar_cane_random_tick`。
`game_tick` に `step("random_ticks", random_tick_pass)`。

## この実装が見つけた実バグ：`with_prop` がブロックの外へ出ていた

サトウキビを書く途中で `reg.with_prop(st, "age", "0")` を呼んだところ、
**age 15 のサトウキビが `minecraft:jukebox`（state 5815）になった**。

```rust
// 誤: 剰余で差分を出し、それを off に足していた
let o = off + (nd + len - digit) % len * mult;
```

`off` はすでに `digit * mult` を含んでいる。`nd < digit` のとき
`(nd + len - digit) % len` は**正の差分**を返すので、本来引くべきところで足していた。
結果、ブロックの状態範囲の**1つ外**へ出て、隣のブロックになった。

```rust
// 正: 差を符号付きで足す
let o = off as i64 + (nd as i64 - digit as i64) * mult as i64;
```

**影響範囲**：`with_prop` はプロパティを設定する全ての箇所で使われる
（軸・向き・年齢・距離・レベル…）。**「現在より低い値へ設定する」たびに**
誤っていた。age 15→0 が分かりやすい例だが、階段の向きを戻す場合なども同様。

**gate**：`setting_a_property_lower_than_its_current_value_stays_inside_the_block`
— サトウキビの age 16値を**全16×16の組合せで双方向**に、加えて階段の
3プロパティ（facing/half/shape）を全組合せで。負の対照は
`state 5815 is minecraft:jukebox` で失敗。

## サトウキビ

vanilla は age 0→15 をランダムtick ごとに1段進め、15を越える tick で
**上の段を置いて age を0に戻す**。したがって**16ランダムtick で1ブロック伸び、
高さ3で止まる** — 決定的なので検証できる。

gate：`sixteen_random_ticks_raise_sugar_cane_one_block`
（15tickでは増えない／16tick目で増える／ageが戻る／3で止まる）。

## RNG を新設した理由

`rand_u64()` は **SystemTime ベース**で、**同じ seed でも結果が変わる**。
ランダムtick は RNG を消費するので、これではテストできない。
`world_rng` は seed 付き・状態を持つ（xorshift64*）。

gate：同じ seed で同じ列／違う seed で違う列／**zero seed が固定点にならない**
（xorshift の失敗モード）／`next_below` の範囲と被覆／状態の往復。

## まだやっていないこと

- **RNG の状態を保存していない。** vanilla は level と一緒に保存するので、
  再起動すると列が先頭に戻る。**vanilla に対する実ギャップ**であり、
  永続化の section を足すまで残る（`saved_state`/`restore` はテスト用にゲート）
- 既存の `rand_u64()` 18箇所は**移していない**（移すとドロップ分布が変わる）。
- 対象 chunk は **hot region + forceload**。vanilla は読み込み済み chunk 全て。
- 苗木・葉の枯れ・草・雪氷は未着手（葉は `distance` プロパティ、
  草は光＝slice 03 を待つ）。
- 作物の既存タイマーは**そのまま**（測ってから決める）。
