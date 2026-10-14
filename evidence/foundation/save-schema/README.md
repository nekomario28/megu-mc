# 保存形式の節を schemagx で検査した

`schemagx` は relational schema の構造欠陥（外部キー循環、解決しない参照、根）を検査する。
保存形式の節と、節どうしの参照をモデル化した。

## 結果：**0 errors**

```
8 tables, 4 foreign keys
roots: world_cell
inspect: 3 problems (0 errors)
  [1] info arrow  orphan-table: nothing references 'arrow' and it references nothing
  [2] info drop   orphan-table: nothing references 'drop' and it references nothing
  [3] info mob    orphan-table: nothing references 'mob' and it references nothing
```

**節の依存に循環は無い。** 根は `world_cell` で、**位置をキーにする節**（`chest` / `furnace` /
`sign` / `falling`）がそこを参照する。`arrow` / `drop` / `mob` はどの節からも参照されないが、
**エンティティは独立しているのでこれは期待どおり**（孤立テーブルではなく、単に辺が無い）。

**節はこの順で書かれ、この順で読める**：`world` → `MET4`（mob・inventory・chest・furnace・
scoreboard）→ `ARW2`（projectile）→ `FAL1`（落下キュー）。**後ろの節が前の節を参照する**ので、
**一方向であり、1パスで書ける。**

## しかし、本当に疑っていた問題は **schemagx の範囲外**だった

**世界は疎なオーバーレイ**である：`base_state` と等しいセルは**保存されない**（黙っていれば
「生成器の答え」を意味する）。一方 `chests` などは**位置をキーに保存される**。

したがって、**セルが消えているのに `chest` の項目が残る**と、
**ロード後に「ブロックの無いチェスト」**ができる — **開くことも壊すこともできず、中身が到達不能**。

**schemagx はこれを検査しない。** 外部キーの**構造**を見ており、**参照先の行が実際に存在するか**は
見ていない（`chest.x → world_cell.local_idx` は型として正しい）。

**コード側には掃除の経路がある**：`set_block_if` は**同一性が変わるとき**
`chests()` / `furnaces()` / `signs()` からその位置の項目を消す（`main.rs:3872`, `13015` 付近）。
**したがって孤児は、`world` と `chests` が非原子的に更新される割り込みでしか生まれない。**

## 次の一手（テストで押さえる）

**schemagx が届かないので、テストにする**：
1. チェストを置き、`chests()` に項目があることを確認
2. `set_block(..., 0)` で壊し、**`chests()` から消えていること**を主張
3. 保存 → 読込の往復で、**存在しないセルを指す項目が残らないこと**を主張

**「構造は正しい」と「参照先が存在する」は別の主張である。** 前者はツール、後者はテスト。
