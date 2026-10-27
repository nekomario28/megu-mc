# 08 — 資産フィールドの一掃（残りの完全なリスト）

Status: 調査完了。slice 03 の「表を1つに集約する」計画を**置き換える**。

## なぜ一掃したか

直近4件の修正は**すべて同じ形**だった：

| 表 | 手書きの被覆 | 食い違い |
|---|---|---|
| `block_hardness` | 356 / 26,684状態 | **26,420** |
| `canHarvest`（`needs_pickaxe`） | 157状態 | 92〜99% |
| `tool_kind` | 157+119+19状態 | **83.9%** |
| `block_drop`（`state_to_item`） | 13状態 | **23,351** |

原因は1つ：**`assets/blocks_1211.json` に既にあるデータを手書きで再導出していた**。
手書きの値は**間違っていない**（資産と一致する）。**圧倒的に不完全**なだけ。

**1件ずつ見つけるのは非効率である。** 資産の全フィールドを一掃し、
再導出しているものを**列挙**すれば、残りが一度に分かる。

## 一掃の結果：資産にあって、サーバーが手で持っている／持っていないもの

### `assets/blocks_1211.json`（ブロック単位）

| フィールド | サーバー側 | 状態 |
|---|---|---|
| `hardness` | 削除して資産から導出 | **完了** |
| `material` | 削除して資産から導出（`tool_kind`） | **完了** |
| `harvestTools` | 資産から導出（`can_harvest`） | **完了** |
| `drops` | 資産から導出（`block_drop`） | **完了** |
| **`stackSize`** | `item_stack_max` が**裸の数値範囲**で再導出 | **未着手** |
| **`resistance`** | **モデルが無い**（爆発抵抗なし） | **未着手** |
| **`emitLight`** | **モデルが無い**（光源が明るさに影響しない） | **未着手** |
| `transparent` / `filterLight` | `state_flags` / `side_flags` 経由で部分的 | 部分 |
| **`boundingBox`** | **モデルが無い**（衝突は `is_solid` の真偽のみ） | **未着手** |
| `diggable` | `hardness < 0` で代替 | 実質OK |

> **注記 (2026-10-25, 出所整理)**: `assets/pblocks.json` と `assets/properties.json` は削除した。両ファイルは別プロジェクトのファイルとバイト一致し、
> しかも 1.21.1 ではないバージョンのデータだった(35,723 状態 vs 1.21.1 の 26,684)。luminance / opacity / 状態フラグ / 面の full 判定は、1.21.1 jar から
> 生成した `assets/block_states_1211.json`(`tools/vanilla_data/`)に移行済み。**`collision_shapes` / `outline_shapes` など形状・摩擦系のフィールドは現在リポジトリに無い**:
> 必要になったら `tools/vanilla_data/Extract.java` を拡張して jar から抽出する(下の記述は削除前の調査記録)。
> *Note: pblocks.json / properties.json were removed (see THIRD_PARTY.md). Per-state light/flags/faces now come from `assets/block_states_1211.json`; shape and friction fields would need an extension of `tools/vanilla_data/Extract.java`.*

### `assets/pblocks.json`（**状態**単位、7.5MB。削除済み — 上の注記参照）

| フィールド | サーバー側 | 状態 |
|---|---|---|
| `state_flags` / `side_flags` | registry が読んでいる（遮蔽・固体） | 使用中 |
| **`collision_shapes`** | **モデルが無い**（階段・ハーフ・フェンス・植物が全て立方体） | **未着手・最大** |
| `outline_shapes` | モデルが無い | 未着手 |
| **`luminance`** | **モデルが無い** | **未着手** |
| `opacity` | `state_flags` 経由で部分的 | 部分 |
| **`blast_resistance`** | **モデルが無い** | **未着手** |
| **`slipperiness`** | **モデルが無い**（氷が滑らない） | **未着手** |
| `velocity_multiplier` / `jump_velocity_multiplier` | モデルが無い | 未着手 |
| `instrument` | モデルが無い（音符ブロック） | 未着手 |
| `map_color` | モデルが無い | 未着手 |
| `piston_behavior` | モデルが無い | 未着手 |

## 計画の訂正：slice 03 の元の設計は不要になった

元の slice 03 は「**5つの表を1つの `Registry` 構造体に集約する**」だった。
**これはもう正しくない。**

**資産がすでに registry である。** 必要なのは新しい抽象化ではなく、
**手書きの再導出を削除して資産を読むこと**。実際にそうした4件は
どれも**行数が増えていない**（合計で純数十行）。

新しい `Registry` 型を作ると、資産という既存の単一所有者の上に
**もう1つ所有者を足す**ことになる。

## 効率化（この一掃で分かった手順の無駄）

1. **測定を毎回アドホックに書いている。** 同じ手順を4回書いた。
   `tools/asset_coverage.py` として置き、1コマンドで任意のフィールドを比較する。
2. **検証が6コマンド。** `tests/verify.sh`（serial → 8thread×3 → wire smoke →
   feature 2種）を1つにまとめ、コミットごとに1回走らせる。
3. **同じ形の修正はまとめられる。** `BReg` にスカラーを1つ足し、accessor を1つ足し、
   消費者を差し替える — という同一手順を5回繰り返す代わりに、
   **スカラー系（`stackSize`, `resistance`, `luminance`, `blast_resistance`,
   `slipperiness`）は1コミットにまとめられる**。
4. **長い散文をシェル heredoc で書かない。** バッククォートが展開されて
   用語が消える事故を3回起こした。`edit` ツールを使う。

## 優先順位（ゲームプレイ影響 ÷ 費用）

| | 項目 | 費用 | 影響 |
|---|---|---|---|
| 1 | `stackSize` | 極小（既存と同形） | 中（インベントリのスタック数） |
| 2 | `slipperiness` | 小 | 小〜中（氷） |
| 3 | `blast_resistance` | 小（爆発の消費者が既にある） | 中（TNT） |
| 4 | `luminance` | 小（保存は）／**消費者が要る** | **大**（光源）— ただし光エンジンが前提 |
| 5 | `collision_shapes` | **大**（形状表現＋移動＋設置） | **最大**（階段・ハーフ・フェンス・植物） |
| 6 | `velocity_multiplier` ほか | 小 | 小 |

**4と5は「データを読む」では終わらない。** 消費者（光エンジン、形状衝突）を
作る必要があり、それぞれ独立した spec に値する。
`collision_shapes` は**このサーバーに残る最大の挙動差**である可能性が高い。

## この一掃が示す、より大きな問題

直近の作業はすべて**vanilla忠実性**であり、**「超軽量化」の証拠は1つも増えていない**。
忠実性の作業は無限に続けられる（上の表がその証拠）。
一方、目標の主張には**測定器が要る**。

**忠実性の作業は、測定器ができるまで打ち切るべきである。**
そうしないと、目標を証明しないまま、目標と関係のない差分だけが積み上がる。
