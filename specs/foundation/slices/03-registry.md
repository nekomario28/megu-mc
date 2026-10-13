# 03 — Block/Item registry と content revision

Status: **gate実装済み + hardness を資産由来に移行済み**。

## 実装済み：hardness を資産から導出（実バグ修正）

`block_hardness` は手書きの `match` だった。**資源はすでに資産にあった**：
`assets/blocks_1211.json` が `hardness` を1060エントリ持っている
（registry が既に読んでいるファイル）。

全状態で比較した結果：

| | |
|---|---|
| 資産が覆う状態 | **26,684** |
| 手書き表が覆う状態 | **356**（1.3%） |
| **食い違う状態** | **26,420** |

手書き表があった356件は**資産と一致**していた。つまり表は「間違い」ではなく
**圧倒的に不完全**で、残りは `_ => 0.6, // unmapped: soft default` に落ちていた。

掘削時間が誤っていた例：`crying_obsidian`（50ではなく0.6）、`netherite_block`、
`diamond_block`/`gold_block`/`emerald_block`、`spawner`、`anvil`、`hopper`、
`chest`、`deepslate` ほか。

**変更の大きさ**：`feature.rs` +14、`main.rs` +6/−36（純 −16行）で
26,420状態が直った。規模対効果は良い方向。

**gate**（資産を**独立に**読んで比較するので、registry経由の再導出ではない）：
- `mining_time_is_the_asset_value_for_every_state` — 2万状態以上を網羅
- `named_blocks_have_their_vanilla_mining_time` — 名前付きの値を直接主張
- 負の対照：既定0.6に戻すと `state ...` と `stone` で失敗
  （[hardness-negative.log](../../../evidence/foundation/registry/hardness-negative.log)）

`dig_seconds` の式（`h * (canHarvest ? 1.5 : 5.0) / speed`）は vanilla のままで、
hardness の出所だけが変わった。

### 残っている同じ形の再導出（次の候補）

資産は `drops` と `harvestTools` も持っている。つまり `state_to_item`（drops）と
`harvest_level_required` / `needs_pickaxe`（harvestTools/material）も
**同じ形の部分的な再導出**である。今回は hardness だけを移した。


## 実装済み：gate（移行の前提であり、それ自体が検査道具）

移行の前に、表どうしが**今**一致しているかを主張するテストを書いた。
移行の等価性ゲートは、移行前の状態を固定していなければ意味がない。

`src/registry_tests.rs` の3テスト：

| テスト | 検査する不変条件 |
|---|---|
| `an_item_places_the_block_it_is_named_after` | `item_to_state(i) = st` なら `name_of(st)` が `i` に解決する |
| `every_block_that_drops_something_is_a_block_the_registry_knows` | `state_to_item(st)` の落とすアイテムがID空間内 |
| `every_hidden_block_name_resolves_to_the_block_it_names` | 隠す名前が全て解決し、かつ**同じ名前のブロック**に解決する |

### 最初のテストは**間違っていた**（記録）

`state_to_item` と `item_to_state` を**逆写像**と仮定して書いたら2件「失敗」した。
**失敗したのはテストであって、表ではなかった。**

- `state_to_item(1) = 35` ：石を掘ると**丸石が落ちる**
- `item_to_state(1) = 1` ：石アイテムは**石を置く**

**別々の正しい関係**である（「このブロックは何を落とすか」と
「このアイテムは何を置くか」）。往復は要求できない。
要求できるのは**配置方向の自己整合性**（置いたブロックがその名前のブロックであること）。

これは `refactor-clean` の「例は問題を記録する」の実例である：
仮説（逆写像）がデータに否定された。

### 静かな穴を1つ塞いだ

`hidden_states` は `filter_map` で解決しており、**解決できない名前を無言で捨てていた**。
アンチXrayが黙って効かなくなる型。名前を `HIDDEN_BLOCK_NAMES` const に抽出し、
「名前の数だけ状態がある」ことをテストで要求した（タイポがテストを落とす）。

### 負の対照

[table-gate-negative.log](../../../evidence/foundation/registry/table-gate-negative.log)：
`item_to_state(1)` を2へ、`iron_ore` を `iron_oar` へ変異させると
`places minecraft:granite, whose name is item 2` と
`iron_oar did not resolve to any block` で失敗する。

### このgateで分かったこと

**3テストとも通る。** 表は正しい不変条件の下で**今は一致している**。
つまり移行前のベースラインが存在し、移行後の等価性をこれで主張できる。

## 着手前の制約は2つとも解けた（下記）

## 実装前に確認した制約

**1. `CachedLookup` は `Copy` な値しか扱えない。**

```rust
pub(crate) fn get_or_compute(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
    if let Some((stored_key, value)) = &self.entry {
        if stored_key == &key { return *value; }   // ← V: Copy が要る
```

炉のキャッシュを `Option<Stack>` にすると `Stack` は `Vec` を持つので通らない。
選択肢：(a) `CachedLookup` を `Clone` 対応にする（SeedProtect側と共有）、
(b) `Comps` を含まない `Copy` な射影だけをキャッシュする。
**(a) は共有コードの変更**なので、影響を確認してから決める。

**2. 可変表を読む箇所は12。** revision の消費者としては十分
（「消費者が2つ以上」の条件を満たす）。

## Why

### 1つのブロックの知識が5つ以上の表に散っている

| 表 | 場所 | 内容 |
|---|---|---|
| `name_to_item` | `:5891` | 名前→ID（587件） |
| `state_to_item` | `:938` | 状態→ドロップ |
| `item_to_state` | `:7947` | アイテム→状態 |
| `block_hardness` | `:608` | 硬さ |
| `hidden_states` | `:3298` | 隠す状態（anti-xray） |
| `state_from_arg` / `base_state` / `worldgen` | 各所 | 名前解決・生成 |

レッドストーン部品を1つ足すには、この複数の表を直す必要がある。
どれか1つを忘れると「置けるが硬さが無い」「隠れない」等の欠落になる。

### 2. 可変な内容をキーに含められず、毎tick引き直している

`CachedLookup` は「可変な表は整合した revision をキーに含めよ」と定めている。
しかし炉のキャッシュキーは **`i64`（アイテムID）だけ**（`:3053`）。

そのため `smelt_out` は可変な datapack レシピを**常に先に全件走査**する：

```rust
fn smelt_out(item, builtin) {
    // Datapack recipes and ingredient tags are mutable: always resolve them first.
    for (ig, o, c, _) in json_smelts().lock().unwrap().iter() {
        if ingr_match(ig, item, 0) { return Some(...) }
    }
    builtin(item).map(...)   // ← ここだけキャッシュされる
}
```

**費用は O(レシピ数 × 炉数) 每tick。** 炉400で datapack レシピが数十件あれば
毎tick数千回の照合になる。キャッシュは不変部分しか覆っていない。

## 契約

**内容の知識に単一の所有者を作り、可変部分には revision を持たせる。**

```rust
/// 不変（起動時固定）: 名前↔ID、硬さ、タグ既定、遮蔽、音、隠す状態
struct Registry { /* ... */ }

/// 可変（datapack）: レシピ、タグ。読み込むたびに revision を上げる。
struct Content { revision: u64, smelts: Vec<...>, tags: HashMap<...> }
```

- ブロック/アイテムの知識は**1箇所で宣言**する。状態範囲・アイテム・硬さ・
  タグ・遮蔽・隠す・音・採掘道具を1つの定義にまとめる。
- datapack 読込は `revision` を上げる。**キャッシュのキーは
  `(content_id, revision)`** とし、`CachedLookup` の要求を構造で満たす。
- `smelt_out` の「常に可変部を全件走査」を、revison付きキーの
  `CachedLookup` に置き換える。**同じ revision の間は1回だけ解決する。**
- 既存の表の内容は**変えない**。移すだけ。値の一致は移行時に検証する。

## 非目標

- ブロックの挙動（tick関数）を1つのtraitに押し込むこと。
  まず**データ**を集約する。挙動は slice 02 の queue と既存関数のまま。
- vanillaの全ブロックを揃えること。足りないものは足りないままにする。
- minecraft-data の自動生成物を丸ごと取り込むこと（出典と版の固定が必要）。

## ゲート

- **移行の等価性**: 旧表と新registryが、全名前・全状態で**同じ答え**を返す。
  587件の `name_to_item`、状態範囲、硬さを総当たりで比較する。
- **revision の正しさ**: datapack を読み込み直すと revision が上がり、
  revision 付きキーのキャッシュが**再計算される**ことを検証する
  （既存の `cached_lookup` テストと炉の回帰テストを維持・拡張）。
- **費用**: `smelt_out` の呼び出しを、炉400×レシピ数十件で前後比較する。
  **x1.10未満は主張しない。**
- 全テスト、CI 4step、wire smoke。

## リスク

- 移行漏れ（表の1つを移し忘れる）。旧関数を薄い委譲にして
  新registryを参照させ、両方を総当たりで突き合わせる。
- revision の上げ忘れ（datapack を読み直したのに古いキャッシュが残る）。
  revision を `load_datapacks` の**唯一の出口**で上げる。
- 出典の欠落。どの表がどこ由来かを定義側にコメントで残す
  （[issue #3](https://github.com/nekomario28/mc-spine/issues/3) の provenance 方針）。
