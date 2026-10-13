# ゲーム要素の実装計画

Status: **slice 01 の1件目（stackSize）完了**。他は計画。

### slice 01 の進捗

| 物性 | 状態 |
|---|---|
| `stackSize` | **完了**（資産から取得）。**5つ目にして初めて「既に正しかった」表** — 228件全て一致 |
| `blast_resistance` | 消費者が無い。`explode_at` は抵抗を見ずに無条件でブロックを壊す。**爆発モデルを書く必要がある** |
| `slipperiness` | 消費者が無い（摩擦が移動に無い） |
| `luminance` / `opacity` | 消費者が無い（slice 03 の光エンジンが消費者になる） |
| `velocity_multiplier` / `jump_velocity_multiplier` | 消費者が無い（ハチミツ・ソウルサンド） |

**規則の確認**：「消費者が居てから物性を足す」。`stackSize` は
`item_stack_max`（呼び出し27箇所）という消費者が**既にあった**ので着手できた。
残りは**消費者を書くことが本体**であり、物性を読むことではない。
つまり slice 02〜05 が先で、物性はその中で一緒に入る。

## 目標と3つの制約

**小規模・低負荷で計算量を減らし、必要なModも使えるサーバー。**
観測可能な挙動は vanilla のまま（[DESIGN.md](../../DESIGN.md)）。

1. **軽量**であることを、同じ仕事で測って示す
2. **vanilla の挙動**を保つ（勝手に近似しない）
3. **証拠**を伴う（測ってから変える／負の対照を取る）

## 支配的な教訓：ブロックの知識は**すでにデータである**

この枝で4つの手書き表を資産由来にした（hardness / canHarvest / tool_kind / drops）。
どれも**手書きの値は間違っておらず、圧倒的に不完全だった**：

| 表 | 手書きの被覆 |
|---|---|
| `block_hardness` | 356 / 26,684 状態 |
| `canHarvest` | 157 状態 |
| `tool_kind` | 295 状態 |
| `block_drop` | 13 状態 |

`assets/pblocks.json`（状態単位、7.5MB）はさらに **`collision_shapes` /
`luminance` / `opacity` / `blast_resistance` / `slipperiness` /
`velocity_multiplier` / `jump_velocity_multiplier` / `instrument` / `piston_behavior` /
`map_color`** を持つ。

**したがってゲーム要素の実装は「挙動を書く」より「データを読み、消費者を1つ書く」になる。**
`python3 tools/asset_coverage.py --list` が残りの完全な一覧を出す。

**規則**：新しいブロック挙動を書く前に、まず
`tools/asset_coverage.py --field <name>` を走らせる。手書き表を書かない。

## 共通部品（消費者が今日2つ以上あるものだけ作る）

| 部品 | 今日の消費者 | 解く問題 |
|---|---|---|
| **ブロック物性レイヤ**（BReg の残りスカラー） | stackSize 1・TNT・氷・ハチミツ・音符・地図 | 手書きの再導出 |
| **ブロック挙動ディスパッチ**（random_tick / on_use / neighbor_changed） | 作物・苗木・葉・サトウキビ・火・雪 | 挙動が `block_updated_inner` に散っている |
| **光エンジン**（luminance + opacity） | 松明・ランタン・溶岩・スカルク・光るしるし | 光源が明るさに反映されない |
| **衝突形状**（collision_shapes / outline_shapes） | 階段・ハーフ・フェンス・ドア・植物・当たり判定 | 全て立方体 |
| **Entity framework**（[slice 04](../foundation/slices/04-entity-framework.md)） | 飛翔体8種 + mob 多数 | 物理4重複 |
| **コンテナ/メニュー** | `open_*` が40箇所 | チェスト/炉/作業台/取引/エンチャ/醸造が別実装 |
| **ディメンション** | ポータル・地形・mob | Nether/End が無い |

## 在庫

**動いている tick システム（15）**：
actors / arrows / block_updates / drops / environment / furnaces / hoppers /
hot_regions / interest / lava / mobs / observers / plugins / tnt / water

**コマンド**：execute, time, difficulty, spawnpoint, setworldspawn, help, tp, say,
kill, gamemode, save-all, stop, clear, effect, xp, setblock, summon, give, enchant,
item, weather, playsound, …

**無いもの**：
ディメンション（Nether/End）／レッドストーン（ワイヤ・リピータ・コンパレータ・
ピストン）／村人・取引・襲撃／mob は5種のみ（chicken・creeper・skeleton・spider・zombie）／
光／衝突形状／落下ブロック／葉の枯れ／エンチャントテーブル・金床・醸造・鍛冶／
ポータル／ベッド／ボート・トロッコ／ビュー距離の設定（PR #6）

## 計画（値 ÷ 費用 の順、各 slice に証拠を要求）

| # | slice | 消費者 | 証拠 |
|---|---|---|---|
| **01** | **ブロック物性レイヤ**（pblocks の残りスカラーを BReg へ） | スタック・爆発・氷・ハチミツ | 資産との全状態一致 + 症状テスト |
| **02** | **ブロック挙動ディスパッチ**（1箇所に集約） | 作物・苗木・葉・サトウキビ | 既存挙動の等価性 |
| **03** | **光エンジン**（luminance + opacity） | 松明・ランタン・溶岩 | 光値の一致 |
| **04** | **衝突形状** | 階段・ハーフ・フェンス・植物 | 形状の一致 |
| **05** | 落下ブロック・葉の枯れ・草の伝播 | 砂・砂利・葉 | 挙動の一致 |
| **06** | Entity framework（[slice 04](../foundation/slices/04-entity-framework.md)） | 飛翔体8 + mob | 物理の同一 |
| **07** | mob をデータ駆動で増やす | etype 表は既にある | 生成・AI |
| **08** | コンテナ/メニューの共通化（`open_*` 40箇所） | チェスト/炉/作業台/取引/エンチャ/醸造 | プロトコル |
| **09** | レッドストーン | ワイヤ・リピータ・コンパレータ・ピストン | 論理の一致 |
| **10** | 村人・取引 | | |
| **11** | ディメンション（Nether/End） | | |
| **12** | full chunk 生成（feature + light）と worldgen 比較 | vanilla 比較 | 同じ仕事の比較 |

## 各 slice の合格条件（この枝の標準）

1. **着手前に測る**（`tools/asset_coverage.py`、または症状の再現）
2. **資産を独立に読む** gate を書く（registry 経由の再導出にしない）
3. **症状そのもの**を固定するテストを書く（「シャベルは土を速くする」型）
4. **負の対照**を取る（変異させて失敗することを確認）
5. `bash tests/verify.sh` を通す
6. 費用がある変更は**連続前後測定**、`x1.10` 未満は主張しない

## 非目標

- **「全部」を一度にやらない。** 上の順序で1つずつ、各々証拠を付ける。
- **手書き表を書かない。** 資産に無いものだけ書く。
- **消費者が居ない抽象化を作らない**（[DESIGN.md](../../DESIGN.md) の原則）。
- vanilla の完全一致を主張しない。**測った範囲だけ**を主張する。
