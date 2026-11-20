# ゲーム要素の実装計画（完全版・R7 GP-0〜GP-7 対応同期済み）

Status: `perf/worldgen-parallel` / `main` 同期（2026-11-20）。**Worldgen / 3ディメンション / 基本照明 / 基本戦闘・コンテナ・流体物理までは実装済み**、残る M2→M4 完全ゲームプレイ領域は [ROADMAP R7 (`GP-0`〜`GP-7`)](../ROADMAP.md#r7--ゲームプレイ完全性と全エンティティ機構拡張m2m4gp-0gp-7) としてトラック化した。

## 目標と3つの制約

**小規模・低負荷で計算量を減らし、必要なModも使えるサーバー。**
観測可能な挙動は vanilla のまま（[DESIGN.md](../../DESIGN.md)）。

1. **軽量**であることを、同じ仕事で測って示す
2. **vanilla の挙動**を保つ（勝手に近似しない）
3. **証拠**を伴う（測ってから変える／負の対照を取る）

## 現状の棚卸し（2026-11-20 実ソース照合済み）

| 領域 | 実装済み・検証済み | ギャップ（`R7 GP-0〜GP-7` で解決対象） |
|---|---|---|
| **エンティティ型 (`GP-0`)** | 名前・protocol ID は `name_to_etype` で 130 種（`0..=129`）解決済み。寸法定義は約 11 種（`src/entity_collision.rs`）、HP テーブル一部、`BaseSpawner` 12 種（`src/spawner.rs`） | `src/entity_registry.rs` へ全 130 種のメタデータ（`MobCategory`・幅高・`eye_height`・`fire_immune`・追従/更新間隔）を統合しデータ駆動化 |
| **Mob AI・自然スポーン (`GP-1` / `GP-2` / `GP-6`)** | 接近・Creeper 着火爆発・繁殖・日光燃焼・溺水・溶岩接触・流体 `travel`・窒息・`maxEntityCramming`（`EntitySpatialGrid`）、バイオームスポーン表対照まで実装 | **`GoalSelector` / A* `PathNavigation` が未実装**。**`NaturalSpawner`（`mob_cap`・`spawn_costs` エネルギー予算・`24/32/128` despawn）** および Nether/End/水棲/ボス Mob 固有 AI が未実装 |
| **光エンジン** | **実装済み**（`0x27`/`0x2a` で実計算の空光・ブロック光を送信・伝播、`src/light_pack_tests.rs` 等で検証済み） | Torch 増分最適化の比較検証のみ残存（Issue `#43`） |
| **衝突・流体・耕地・落下** | 流体 `travel`・水浮力・溶岩押し流し（Nether `ultraWarm` 含む）・`Entity.move` 衝突解決・耕地 `trample`・落下ブロック実装済み | 階段/ハーフ/フェンス等の任意非フルブロック精密形状（`Extract.java` 拡張＋[horizon-gameplay](../horizon-gameplay/README.md)）を順次拡大 |
| **レッドストーン (`GP-3`)** | ピストン・オブザーバー・ホッパー・ディスペンサー等の個別動作は一部存在 | **ワイヤ信号伝播（0..=15・段差昇降・遮断）・トーチ（2GT・burnout）・リピータ（遅延・横ロック）・コンパレータ（比較/減算・コンテナ充填信号）・Quasi-Connectivity が未実装** |
| **プレイヤー機構 (`GP-4` / `GP-5`)** | 移動・破壊/設置・2×2 クラフト・精錬・チェスト/樽/エンダーチェスト（遅延 LootTable/水没含む）・戦闘/死亡/リスポーン・盾・釣り・基本エンチャント/空腹/XP（`tests/m2_gameplay_wire.py` PASS） | 村人 AI/職業/取引・レイド、エンチャントテーブル 3 択、金床合成、醸造台/ポーション拡張、鍛冶台、経験値オーブ吸収、空腹消耗度精密化 |
| **ディメンション・構造物** | **実装済み**（`P2-3-S0〜S4` で Overworld/Nether/End 全ストア分離・`0x2b`/`0x47` ポータル遷移完了、`tests/dimension_wire.py` PASS。構造物 19 チャンク 0-diff 昇格済み） | 残存 HOLD/未検証構造物（Batch B4）、エンドラ討伐フロー・ワールドボーダー |
| **コマンド・進捗 (`GP-7`)** | Brigadier ツリー（`src/brigadier.rs`）・約 25 コマンドディスパッチ | `/fill`, `/clone`, `/execute`, `/scoreboard`, `/advancement`, `/worldborder`, `/locate` 等の残存コマンドと進捗通知 |

**つまり「基盤（3ディメンション・光・流体・衝突コア・NBT・wire 経路）と名前表（1,061 ブロック / 1,333 アイテム / 130 エンティティ）は揃っており、次の主戦場が `R7 (GP-0〜GP-7)` のゲームプレイ振る舞い実装」という段階にある。**

## 支配的な教訓

**ブロックの知識はすでにデータである。** この枝で5つの手書き表を資産由来にした
（hardness / canHarvest / tool_kind / drops / stackSize）。どれも
**手書きの値は間違っておらず、圧倒的に不完全だった**（356/26,684、13/23,364 等）。

> **注記 (2026-10-25, 出所整理)**: `assets/pblocks.json` と `assets/properties.json` は削除した。両ファイルは別プロジェクトのファイルとバイト一致し、
> しかも 1.21.1 ではないバージョンのデータだった(35,723 状態 vs 1.21.1 の 26,684)。luminance / opacity / 状態フラグ / 面の full 判定は、1.21.1 jar から
> 生成した `assets/block_states_1211.json`(`tools/vanilla_data/`)に移行済み。**`collision_shapes` / `outline_shapes` など形状・摩擦系のフィールドは現在リポジトリに無い**:
> 必要になったら `tools/vanilla_data/Extract.java` を拡張して jar から抽出する(下の記述は削除前の調査記録)。
> *Note: pblocks.json / properties.json were removed (see THIRD_PARTY.md). Per-state light/flags/faces now come from `assets/block_states_1211.json`; shape and friction fields would need an extension of `tools/vanilla_data/Extract.java`.*

`assets/pblocks.json`(削除済み。上の注記参照)は状態ごとに `collision_shapes` / `luminance` / `opacity` /
`blast_resistance` / `slipperiness` / `velocity_multiplier` / `instrument` /
`piston_behavior` / `map_color` を持つ。

**規則**：新しい挙動を書く前に `python3 tools/asset_coverage.py --field <name>` を
走らせる。**手書き表を書かない。** 資産に無いもの（挙動そのもの）だけ書く。

## 欠けているもの（一覧と `R7` への対応）

### A. ブロック挙動（データは既にある）

| 要素 | データ源 | 消費者・進捗 |
|---|---|---|
| **光**（松明・ランタン・溶岩・スカルク） | `luminance` + `opacity` | 光エンジン（**実装済み**／Issue `#43` で増分最適化比較） |
| **衝突形状**（階段・ハーフ・フェンス・ドア・植物） | 1.21.1 jar から `Extract.java` 拡張抽出 | 移動・設置（部分実装・継続拡張） |
| **落下ブロック**（砂・砂利・コンクリートパウダー） | 名前 | 落下tick（**実装済み**／残ギャップは [evidence/gameplay/falling-blocks](../../evidence/gameplay/falling-blocks/README.md)） |
| **葉の枯れ・苗木成長** | `distance` プロパティ / `tree.rs` | ランダムtick（土台済み） |
| **草の伝播・雪氷** | バイオーム温度 + 光 | ランダムtick |
| **水流・溶岩流・耕地 trample** | 状態プロパティ / `landing.rs` | **実装済み**（`flowing_fluid.rs`・`entity_fluid.rs`・3次元対応済み） |
| **水没（waterlogging）** | 状態プロパティ | 設置・流体・waterlogged chest 実装済み（全面統合継続） |
| **ベッド・火・音符・爆発抵抗・滑り** | `blast_resistance` / `slipperiness` 等 | `R7 GP-3 / GP-5 / GP-6` で段階接続 |
| **感圧板・ボタン・レバー** | 名前 | レッドストーン伝播（`R7 GP-3`） |

### B. レッドストーン（`R7 GP-3` — ワイヤ伝播・各種ゲートが未実装）

ピストン・ディスペンサー・ホッパー・オブザーバーの基本枠はあるが、
**ワイヤ（0..=15 減衰・段差）・トーチ（2GT・burnout）・リピータ（遅延・横ロック）・コンパレータ（比較/減算・充填率）・Quasi-Connectivity の伝播エンジン**が未実装（`R7 GP-3`）。

### C. エンティティ & AI（`R7 GP-0` / `GP-1` / `GP-2` / `GP-6`）

130 種（`0..=129`）の protocol ID は `name_to_etype` で定義済みだが、メタデータのデータ駆動レジストリ（`GP-0`）、A* 経路探索と `GoalSelector`（`GP-1`）、`NaturalSpawner`（`GP-2`）、および Nether / End / 水棲 / 中立・ペット / 村人 / ボス / 乗物の固有行動（`GP-4` / `GP-6`）が未実装。

### D. プレイヤー機構（`R7 GP-4` / `GP-5`）

基本の移動・クラフト・精錬・チェスト・盾・釣り・戦闘/死亡・基本食料/XP は `tests/m2_gameplay_wire.py` で動作確認済み。
残件：**エンチャントテーブル 3 択生成・金床・醸造台・ポーション拡張・鍛冶台・村人取引・襲撃・ベッドリスポーン・経験値オーブ・空腹消耗度**。

### E. ディメンション（`P2-3` 実装済み ／ 残件は `GP-6` エンドラ等）

**Overworld / Nether / End の 3 ディメンション生成・保存・チャンク配信・ポータル移動は `P2-3-S0〜S4` で実装済み**（`tests/dimension_wire.py` PASS）。Nether/End 固有 Mob AI（Blaze/Ghast/Piglin/Enderman/Shulker/Ender Dragon）は `R7 GP-6` で追加する。

### F. 世界機構（`R7 GP-2` / `GP-6` / `GP-7`）

天候・`BaseSpawner` は実装済み、構造物は 19 チャンク 0-diff 昇格済み。残件：**`NaturalSpawner` 完成（`GP-2`）・村/POI/Raid（`GP-4`）・ワールドボーダー（`GP-7`）・Deep Dark / Warden 等**。

### G. コマンド・進捗（`R7 GP-7`）

`clone` `fill` `execute` `data`（一部）`scoreboard` `tag`（一部）`team` `bossbar`
`particle` `worldborder` `forceload` `locate` `recipe` `advancement` `attribute`
`damage` `place` `random` `reload` `spectate` `spreadplayers` `trigger`。

## 順序（`R7` 並列トラック対応）

| # | slice / R7 ID | 状態 | 価値 |
|---|---|---|---|
| 01 | ブロック物性レイヤ | `stackSize`・`block_states_1211.json` 完了 | 土台 |
| 02 | ブロック挙動ディスパッチとランダムtick | RNG・サンプラ・サトウキビ完了 | 土台 |
| 03 | **[光エンジン](slices/03-light-engine.md)**（luminance + opacity） | **実装済み**（Issue `#43` で増分最適化比較のみ残存） | **大** |
| 04 | **衝突形状** | 基本 AABB/`entity_collision` 完了、任意形状は `Extract.java` 拡張予定 | **最大** |
| 05 | 落下ブロック・流体 `travel` | **実装済み**（残ギャップは [falling-blocks](../../evidence/gameplay/falling-blocks/README.md)） | 中 |
| 06 (`GP-0`) | Entity Framework & 130種レジストリ（`src/entity_registry.rs`） | **R7 で着手** | 土台 |
| 07 (`GP-1/2/6`) | `GoalSelector` / A* 経路探索・`NaturalSpawner`・残存 Mob 群 | **R7 で着手** | **大** |
| 08 | コンテナ/メニューの共通化 | 主要コンテナ完了、メニュー共通化継続 | 中 |
| 09 (`GP-3`) | **レッドストーン伝播**（`src/redstone.rs`） | **R7 で着手** | **大** |
| 10 (`GP-4`) | 村人 AI・POI・取引・襲撃 | 未着手 | 中 |
| 11 (`P2-3`) | **ディメンション（Overworld/Nether/End）** | **基盤実装済み**（固有 Mob は `GP-6`） | **大** |
| 12 (`GP-5`) | プレイヤー高度機構（エンチャ台・金床・醸造・ポーション） | 未着手 | 中 |
| 13 (`GP-7`) | ワールドボーダー・進捗・残余世界機構 | 未着手 | 小 |
| 14 (`GP-7`) | 残存コマンド | 未着手 | 小 |
| 15 (`P2-1`) | full chunk / 全構造物 worldgen 一致 | 19 構造物チャンク 0-diff 昇格済み（Batch B4 進行中） | 目標 |

## 各 slice の合格条件（この枝の標準）

1. **着手前に測る**（`tools/asset_coverage.py`、または症状の再現）
2. **資産を独立に読む** gate（registry 経由の再導出にしない）
3. **症状そのもの**を固定するテスト
4. **負の対照**（変異させて失敗することを確認）
5. `bash tests/verify.sh` — **`&&` で繋ぐ**（止まらないゲートはゲートではない）
6. 費用がある変更は**連続前後測定**。`x1.10` 未満は主張しない
7. **fixture が測るコードに到達しているか確認する**
   （`assets/` と `datapacks/` の欠落で3件の測定を無効にした）
8. 測定が無効と判明したら、**主張を明示的に撤回する**

## 非目標

- **「全て」を一度にやらない。** 上の順序で1つずつ、各々証拠を付ける。
- **手書き表を書かない。** 資産に無いものだけ書く。
- **消費者が居ない抽象化を作らない**（[DESIGN.md](../../DESIGN.md) の原則）。
- vanilla の完全一致を主張しない。**測った範囲だけ**を主張する。
