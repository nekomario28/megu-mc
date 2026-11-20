# ゲーム要素の実装計画

Worldgen、3ディメンション、基本照明・戦闘・コンテナ・流体物理に続く機能は [ROADMAP R7 (`GP-0`〜`GP-7`)](../ROADMAP.md#r7--ゲームプレイ完全性と全エンティティ機構拡張m2m4gp-0gp-7) で扱う。R7 はレーンごとに、ロジックの実装、通常サーバーへの接続、Vanilla 1.21.1 との挙動照合を確認する。

## 目標と3つの制約

**小規模・低負荷で計算量を減らし、必要なModも使えるサーバー。**
観測可能な挙動は vanilla のまま（[DESIGN.md](../../DESIGN.md)）。

1. **軽量**であることを、同じ仕事で測って示す
2. **vanilla の挙動**を保つ（勝手に近似しない）
3. **証拠**を伴う（測ってから変える／負の対照を取る）

## 現状の棚卸し（2026-11-20 実ソース照合済み）

| 領域 | 現在の実装と検証範囲 | ギャップ（`R7 GP-0〜GP-7` で解決対象） |
|---|---|---|
| **エンティティ型 (`GP-0`)** | [130 型レジストリ](../../src/entity_registry.rs)と既存の Mob 処理 | 衝突寸法、HP、スポーナーの参照を順にレジストリへ接続 |
| **Mob AI・自然スポーン (`GP-1` / `GP-2` / `GP-6`)** | 基本 Mob の移動・戦闘・環境挙動が稼働。経路探索、自然スポーン、一部の個別 Mob はモジュールと単体テストあり | ワールドや tick への接続、未対応ゴールと Mob の実装、挙動対照 |
| **光エンジン** | **実装済み**（`0x27`/`0x2a` で実計算の空光・ブロック光を送信・伝播、`src/light_pack_tests.rs` 等で検証済み） | Torch 増分最適化の比較検証のみ残存（Issue `#43`） |
| **衝突・流体・耕地・落下** | 流体 `travel`・水浮力・溶岩押し流し（Nether `ultraWarm` 含む）・`Entity.move` 衝突解決・耕地 `trample`・落下ブロック実装済み | 階段/ハーフ/フェンス等の任意非フルブロック精密形状（`Extract.java` 拡張＋[horizon-gameplay](../horizon-gameplay/README.md)）を順次拡大 |
| **レッドストーン (`GP-3`)** | 既存の隣接給電・個別ブロック動作と、[伝播シミュレーション](../../src/redstone.rs)の単体テスト | ワールドのブロック状態と tick へ接続し、回路で照合 |
| **プレイヤー機構 (`GP-4` / `GP-5`)** | 基本操作・収納・戦闘・空腹・XP 回収が稼働。村人取引・襲撃と高度プレイヤー機構はモジュールと単体テストあり | 村人、メニュー、醸造 tick、効果適用、空腹処理へ接続 |
| **ディメンション・構造物** | **実装済み**（`P2-3-S0〜S4` で Overworld/Nether/End 全ストア分離・`0x2b`/`0x47` ポータル遷移完了、`tests/dimension_wire.py` PASS。構造物 19 チャンク 0-diff 昇格済み） | 残存 HOLD/未検証構造物（Batch B4）、エンドラ討伐フロー・ワールドボーダー |
| **コマンド・進捗 (`GP-7`)** | 既存の `/fill`、`/execute`、`/scoreboard`、`/advancement` 等が部分動作。[新しい評価器](../../src/world_mechanisms.rs)は単体テストあり | 既存コマンドの拡張、新規コマンドの接続、進捗パケット配信 |

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

### B. レッドストーン（`R7 GP-3`）

通常サーバーの隣接給電を入口に、伝播、遅延、受け手への状態反映を接続する。回路の合否は入力操作から更新 tick、最終ブロック状態まで追う。

### C. エンティティ & AI（`R7 GP-0` / `GP-1` / `GP-2` / `GP-6`）

Mob の種類を解決するレジストリ（`GP-0`）から既存の参照を順に切り替える。経路探索とゴール（`GP-1`）、自然スポーンと despawn（`GP-2`）はそれぞれ tick に接続し、個別 Mob（`GP-6`）は小さなグループごとに挙動を照合する。

### D. プレイヤー機構（`R7 GP-4` / `GP-5`）

基本の移動・クラフト・精錬・チェスト・盾・釣り・戦闘/死亡・基本食料/XP は `tests/m2_gameplay_wire.py` で動作確認済み。
高度プレイヤー機構はメニューの入力から結果・消費・保存まで照合する。村人取引と襲撃は、実体の状態更新とプレイヤーから見える結果を確認する。ベッドリスポーンと空腹の精密化も残る。

### E. ディメンション（`P2-3` 実装済み ／ 残件は `GP-6` エンドラ等）

**Overworld / Nether / End の 3 ディメンション生成・保存・チャンク配信・ポータル移動は `P2-3-S0〜S4` で実装済み**（`tests/dimension_wire.py` PASS）。Nether/End 固有 Mob AI（Blaze/Ghast/Piglin/Enderman/Shulker/Ender Dragon）は `R7 GP-6` で追加する。

### F. 世界機構（`R7 GP-2` / `GP-6` / `GP-7`）

天候・`BaseSpawner` は実装済み、構造物は 19 チャンク 0-diff 昇格済み。残件：**`NaturalSpawner` 完成（`GP-2`）・村/POI/Raid（`GP-4`）・ワールドボーダー（`GP-7`）・Deep Dark / Warden 等**。

### G. コマンド・進捗（`R7 GP-7`）

既存のコマンド処理へ対象の評価器を接続し、権限、コマンド結果、世界やプレイヤー状態への反映を確認する。進捗は達成判定に加えてクライアントへの配信を確認する。対象コマンドは [ROADMAP の GP-7](../ROADMAP.md#r7--ゲームプレイ完全性と全エンティティ機構拡張m2m4gp-0gp-7) を参照。

残件の棚卸しには、計画に挙げたものに加え、`data`、`tag`、`team`、`bossbar`、`particle`、`forceload`、`recipe`、`attribute`、`place`、`random`、`reload`、`spectate`、`trigger` の未対応範囲を含める。

## 順序（`R7` 並列トラック対応）

| # | slice / R7 ID | 状態 | 価値 |
|---|---|---|---|
| 01 | ブロック物性レイヤ | `stackSize`・`block_states_1211.json` 完了 | 土台 |
| 02 | ブロック挙動ディスパッチとランダムtick | RNG・サンプラ・サトウキビ完了 | 土台 |
| 03 | **[光エンジン](slices/03-light-engine.md)**（luminance + opacity） | **実装済み**（Issue `#43` で増分最適化比較のみ残存） | **大** |
| 04 | **衝突形状** | 基本 AABB/`entity_collision` 完了、任意形状は `Extract.java` 拡張予定 | **最大** |
| 05 | 落下ブロック・流体 `travel` | **実装済み**（残ギャップは [falling-blocks](../../evidence/gameplay/falling-blocks/README.md)） | 中 |
| 06 (`GP-0`) | Entity Framework & 130種レジストリ（`src/entity_registry.rs`） | レジストリあり、実行時参照を段階移行 | 土台 |
| 07 (`GP-1/2/6`) | `GoalSelector` / A* 経路探索・`NaturalSpawner`・残存 Mob 群 | ロジックあり、tick 接続と対応範囲の拡大が残る | **大** |
| 08 | コンテナ/メニューの共通化 | 主要コンテナ完了、メニュー共通化継続 | 中 |
| 09 (`GP-3`) | **レッドストーン伝播**（`src/redstone.rs`） | シミュレーションあり、ワールド未接続 | **大** |
| 10 (`GP-4`) | 村人 AI・POI・取引・襲撃 | ロジックあり、通常サーバー未接続 | 中 |
| 11 (`P2-3`) | **ディメンション（Overworld/Nether/End）** | **基盤実装済み**（固有 Mob は `GP-6`） | **大** |
| 12 (`GP-5`) | プレイヤー高度機構（エンチャ台・金床・醸造・ポーション） | ロジックあり、メニュー・tick 未接続 | 中 |
| 13 (`GP-7`) | ワールドボーダー・進捗・残余世界機構 | ロジックあり、適用・配信が残る | 小 |
| 14 (`GP-7`) | 残存コマンド | 既存コマンドは一部稼働、新しい評価器の接続が残る | 小 |
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
