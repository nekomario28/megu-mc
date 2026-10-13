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

## 費用の測定（重要 — 実装が高すぎる）

`evidence/time-contract/profile_tick.py`（2 run、release）で stage 別に見ると：

| シナリオ | tick p50 | `random_ticks` | 全stage合計 | 占有 |
|---|---|---|---|---|
| empty | 1,237ns | 39.3µs | 807.8µs | 5% |
| idle_actor | 33,077ns | **22,996.3µs** | 32,782.2µs | **70%** |
| dense_actors | 259,345ns | 76,504.6µs | 259,439.0µs | 30% |
| entity_heavy | 372,509ns | 26,950.4µs | 373,705.2µs | 7% |

（stage は 1000 tick あたりの ns。`idle_actor` で 23,000µs/1000tick = **23µs/tick**、
tick p50 は 33µs。つまり**ランダムtick が tick の約7割**。）

**これは「超軽量化」に反する。** このままでは入れられない。

### 原因

`ticked_chunks()` = hot region。1 chunk あたり `ticked_sections()` が
**最低2区間**（surface 帯）を返すので **6回/tick/chunk** の `on_random_tick`。
`on_random_tick` 1回が約1µs：

1. **`block_at` が毎回 `world()` の Mutex を取る**
2. **`reg.name_of(st) == "minecraft:sugar_cane"` という文字列比較**で分岐

実測（1 chunk・編集0）: chunk 1、section 2、`on_random_tick` **6回/tick**。
つまり**費用は chunk 数 × 区間数 × 3 に比例**し、1回あたりが重い。

### 次の課題（測定済みなので順序が決まる）

1. **ブロックごとの挙動ID を事前計算する**（u8）。`name_of` の文字列比較を
   **整数比較**に置き換える。`BReg` に `behaviour: Vec<u8>` を足し、
   `name_of` ではなくIDで分岐する
2. **1回のパスで `world()` を1度だけ取る**。 `block_at` を内側で呼ばず、
   ガードを保持したまま読む（`set_block` を呼ぶ経路は再入になるので分離が要る）
3. そのうえで**連続前後測定**（`x1.10` 未満は主張しない）

**3が済むまでこの機能は「入った」とは言えない** — 現在の費用は
[tick の70%]であり、記録として残す。

## 最適化と連続前後測定（訂正あり）

**前の記録は誤っていた。** 「1回あたり約1µs」と書いたが、実際は約32nsで、
費用の正体は**サンプル数**だった：

- `refresh_hot` は中心から半径5 → **121 chunk** が hot
- `ticked_sections` は chunk あたり最低2区間（surface 帯）
- よって **121 × 2 × 3 = 726 サンプル/tick**

そして vanilla は**全区間（24）**を標本するので、同じ121 chunk なら
**8,712 サンプル/tick**。つまり我々の実装は vanilla より**12倍少ない**。
「tick の70%」は欠陥ではなく**機能固有の費用**であり、記録を訂正する。

### 入れた最適化

1. **ブロックごとの挙動ID（u8）** を `BReg` に持たせ、`name_of` の
   **文字列比較を整数比較**に（`behaviour_of(st)` vs `B_SUGAR_CANE`）
2. **`breg()` をパスごとに1回**（サンプルごとではなく）
3. 共通ケース（B_NONE）は**ゼロとの比較1回**で済む

### 前後（連続測定、release、2run 平均）

| シナリオ | random_ticks 前 | 後 | 比 | tick 前 | tick 後 | 比 |
|---|---|---|---|---|---|---|
| empty | 37.8µs | 18.6µs | 2.03x | 1,238ns | 1,218ns | 1.02x |
| idle_actor | 22,746µs | 18.4µs | **1237x** | 32,747ns | 10,159ns | **3.22x** |
| active_furnaces_pack | 27,110µs | 18.3µs | 1478x | 42,300ns | 15,274ns | **2.77x** |
| entity_heavy | 26,542µs | 18.5µs | 1434x | 350,778ns | 323,866ns | 1.08x |
| dense_actors | 76,378µs | 17.6µs | 4341x | 258,258ns | 183,276ns | 1.41x |

### この比は**まだ主張しない** — 説明できない定数がある

**最適化後は全シナリオで約18µs一定**である（`empty` でも 18.6µs）。
`empty` は actor 0 なので hot region は空のはずで、pass は何もしない。
それでも 18µs かかるということは、**費用の大半が `ticked_chunks()` 自体**
（region ロック + forceload ロック + Vec 構築）である可能性が高い。

さらに **1237倍** は文字列比較の差としては大きすぎる。**前の版で何が
起きていたかを説明できていない。** したがって：

- **tick 比（x2.77〜x3.22）は測った事実として記録する**
- **random_ticks の x1237 は原因不明として記録し、主張しない**
- 次にやること：`ticked_chunks()` の費用を単独で測る。
  定数18µs が説明できれば、比が意味を持つ

## fixture が機能を測っていなかった（最重要の欠陥）

**ランナーは scratch directory で fixture を走らせるが、`assets/` を用意していなかった。**
そのため fixture の中では `BReg::load` が失敗して **`breg()` が `None`** になり、
**registry に依存する経路は全て黙って何もしない**。

つまり：

- `let Some(reg) = breg() else { return }` の**前後どちらの版も、実質何もしていなかった**
- 私はそれを知らずに **x1237** という比を出しかけた
- 「どのシナリオでも約18µs一定」という**まさにその異常**が唯一の手がかりだった
  （それは**ハーネスの stage 計測オーバーヘッド**であって、機能の費用ではない）

**修正**：`profile_tick.py` が scratch dir に `assets/` と `datapacks/` を symlink する。
これはこの枝の原則そのもの — **「fixture は変更したコードに到達しなければならない」**
（[audit-performance の測定規律](/home/yuu/.agents/skills/audit-performance/references/measurement-discipline.md) に
同じ失敗を既に書いていた）。**書いてあるだけで、自分の fixture を確認していなかった。**

### 修正後の実測（release、2run）

| シナリオ | random_ticks | tick p50 | 占有 |
|---|---|---|---|
| empty | 36.6µs | 1,232ns | 3% |
| idle_actor | **59,442.7µs** | 69,391ns | **86%** |
| dense_actors | 198,229.0µs | 382,408ns | 52% |
| entity_heavy | 77,066.1µs | 402,932ns | 19% |

（1000 tick あたり。`idle_actor` で **59µs/tick**、726 サンプルなので **81ns/サンプル**）

### この数字の読み方

**tick の86%** ではあるが、**tick 自体が 69µs** であり、目標は **50ms = 50,000µs**。
つまり **フレーム予算の 0.12%**。比率が高いのは**他がほぼ無料だから**であって、
絶対値は小さい。**「tick の86%」だけを引用すると誤解を生む。**

対して vanilla は同じ121 chunk で **8,712 サンプル/tick**（全区間）。
81ns/サンプルなら換算 **約700µs/tick**、vanilla の 50ms 予算の 1.4%。

### 前後の比について

**`assets/` 欠落時の前後比較（x3.22 等）は、registry に依存する経路では無効である。**
tick 全体は他の stage も含むので 0 ではないが、**ランダムtick の費用比较としては
使えない**。有効な比較はこの修正の後に取り直す必要がある。

## 前後比較の取り直し（結論：最適化の効果は無かった）

worktree で最適化前（`0c52559^` の `src/main.rs`/`feature.rs`/`tick_tests.rs`）を建て、
**fixture を直した状態で**連続測定（release、2run）：

| シナリオ | rt 前 | rt 後 | rt 比 | tick 前 | tick 後 | tick 比 |
|---|---|---|---|---|---|---|
| empty | 37.8µs | 36.6µs | 1.03x | 1,232ns | 1,232ns | 1.00x |
| idle_actor | 59,546.6µs | 59,442.7µs | **1.00x** | 69,451ns | 69,391ns | **1.00x** |
| active_furnaces_pack | 77,671.8µs | 78,226.1µs | 0.99x | 92,750ns | 92,354ns | 1.00x |
| dense_actors | 198,589.3µs | 198,229.0µs | 1.00x | 382,143ns | 382,408ns | 1.00x |
| entity_heavy | 77,453.2µs | 77,066.1µs | 1.01x | 403,438ns | 402,932ns | 1.00x |

**最適化（挙動ID + `breg()` の巻き上げ）に測定可能な効果は無い。**

- 前に出した **x3.22 / x2.77 は誤り**だった。`assets/` 欠落の fixture で測っており、
  両側とも pass が走っていなかった。差は別のもの（ノイズか他stage）だった
- 挙動ID版は**残す**。整数ディスパッチと「挙動を1行で足せる」構造は
  アーキテクチャとして良く、害も無い。ただし**「速くした」とは主張しない**
- 81ns/サンプルの内訳（`block_at` の mutex/ハッシュ/fallback か、
  ハーネスの計測オーバーヘッドか）は**まだ分かっていない**

**この一連の失敗が示すこと**：私は測定規律をスキルに書いておきながら、
**自分のfixtureが測るコードに到達しているかを確認していなかった**。
「異常な定数（どのシナリオでも18µs）」が唯一の手がかりで、
それを説明しようとして初めて見つかった。
