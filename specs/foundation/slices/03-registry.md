# 03 — Block/Item registry と content revision

Status: 設計のみ。着手前の制約を2件確認した（下記）。

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
