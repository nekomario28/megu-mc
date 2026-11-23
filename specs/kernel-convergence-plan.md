# Kernel Convergence Theory & megu 1.21.1 Completion Plan

> **状態訂正 (2026-11-18, P2-1/P2-3 監査レーンによる実読):** この文書の Part 2.2/2.3 の波計画は
> HEAD (`03744b1`) より古い状態を記述している。**16アルゴリズム型すべてが現在ライブな dispatch 先
> (`src/structure_pipeline.rs:226-256`) と専用 owner、 committed native oracle TSV を持つ**
> (`evidence-dispatch.txt` 参照; 実装の棚卸しはローカル監査ブループリントを参照)。
> Kernel A/Wave 3 を「未着手」として P2-1 の規模推定に使わないこと。残る実務は
> (a) 共通の書込側契約（`StructurePiece.placeBlock` の `SHAPE_CHECK_BLOCKS` マークが
> template/jigsaw 経路で欠落 — Step 3レーンが対応中）、(b) 34定義中24の live-chunk 受入。

## Part 1: 汎用「カーネル収束理論 (Kernel Convergence Theory)」（他プロジェクト転用テンプレート）

大規模な移植（Porting）・リライト・互換実装プロジェクトにおいて、**「基盤の9割以上は揃っているのに、残りの個別機能・エッジケースの消化が終わらない」** という現象を打破し、最短距離で収束させるための設計・進行フレームワーク。

### 1. 9割完成後に進捗が止まる「4つの構造的ブレーキ」

1. **ブレーキ①：N個の仕様を「N個の個別実装」として直列処理している**
   - 表面上の仕様（例：34種の構造物、60種の機能、100種のAPIエンドポイント）を1つずつ専用ハーネス・専用ロジックで実装すると、作業量が $O(N)$ で発散する。
   - **解法（カーネル因数分解）**: 全ての仕様を **「少数の共通実行エンジン（Execution Kernels）＋データ定義」** に因数分解する。多くのシステムでは、数十〜数百の仕様は **3〜4個の共通カーネル** の組み合わせに還元できる。
2. **ブレーキ②：初期フェーズの「安全制限（スコープロック）」が汎用カーネルを塞いでいる**
   - 最初の1個（パイロット実装）を通すために入れた「この型しか通さない」「このフラグ以外はエラーにする」という fail-closed ガードが、そのまま共通エンジンの入り口に残っていると、後続の仕様がすべてブロックされる。
   - **解法（段階的ロック解除）**: パイロット固有の契約テストは別エントリポイントに隔離し、共通カーネル自体は全仕様で使うプリミティブ（状態変換・副作用・ペイロード）を一括解放する。
3. **ブレーキ③：非決定性（実行順・スレッド競合）を人間が手で逆算している**
   - リファレンス実装（純正・旧システム）がマルチスレッドや非同期キューで動く場合、結果の差分が「ロジックのバグ」なのか「実行順の揺れ」なのかを切り分けるだけで膨大な時間が溶ける。
   - **解法（実行順トレースの機械的注入）**: リファレンス側に軽量な順序ロガー（1行フック）を埋め込み、新実装側にその順序トレースをそのまま再生する決定論的リプレイ入力（例：`POP_ORDER`）を持たせる。
4. **ブレーキ④：「生成時（バッチ）」と「実行時（ランタイム）」の副作用が二重実装になっている**
   - 形状接続・初期インベントリ展開・初期ティック予約などを機能ごとに個別実装すると、バグもテストも $N$ 倍になる。
   - **解法（Post-Placement / Commit Finalizer の一元化）**: 全ての生成・配置パスが単一の `Finalizer`（ペイロードステージング＋境界形状更新＋ティック予約）に合流するパイプラインを作る。

---

### 2. カーネル収束の 5 ステップ手順（他プロジェクトでも使える汎用手順）

```
Step 1: 全量インベントリ監査 (Automated Coverage Audit)
  └─ データ定義・AST・設定ファイルを全走査し、「未対応プリミティブ」の正確な残数を出す。
Step 2: アーキタイプ因数分解 (Kernel Factorization)
  └─ 残存仕様を「どの共通カーネルに乗るか」で3〜4グループに分類する。
Step 3: 共通カーネルのロック解除 (Unleash Shared Kernels)
  └─ 個別仕様を書く前に、共通カーネルの制限解除と共通 Finalizer への接続を完了させる。
Step 4: リファレンス順序の決定論化 (Deterministic Trace Replay)
  └─ 実行順・乱数シード・タイムスタンプ等の環境変数をトレースから自動注入し、手作業の推測をゼロにする。
Step 5: アーキタイプ単位の一括バッチ開通 (Batch Kernel Rollout)
  └─ 1仕様ずつではなく、同じカーネルを共有する仕様群をサブエージェント並列で一括開通する。
```

---

## Part 2: `megu` 1.21.1 最短完遂計画（実行ロードマップ）

### 1. 現在地の定量監査（[`tools/p0_2_audit.py`](/home/yuu/megu/tools/p0_2_audit.py) / [`tools/feature_type_coverage.py`](/home/yuu/megu/tools/feature_type_coverage.py)）

- **Density Function / Noise / Surface Rule / Condition (Overworld, Nether, End)**: 未対応 **0件**
- **Placement Modifier (全18種)**: 未対応 **0件**
- **Configured Feature (全61種 / 238定義)**:
  - Overworld / Nether: 未対応 **0件**
  - End: 到達可能な未対応は `minecraft:end_spike` **1件のみ**（`bonus_chest` は通常生成から未到達）
- **Template / NBT / 状態回転 / StructureSet 候補計算**:
  - 全 1,180 `.nbt` テンプレート読込・26,684 BlockState 回転変換・20 StructureSet のチャンク候補計算まで実装済み

### 2. 全34構造物の 3 カーネル因数分解

| カーネル | 対象構造物（計34定義） | 共通エンジン |
|---|---|---|
| **Kernel A: 単一/少数 Template + Processor（12定義）** | `nether_fossil`, `igloo`, `shipwreck`, `shipwreck_beached`, `ocean_ruin_cold`, `ocean_ruin_warm`, `ruined_portal` (7種) | [`src/structure_template.rs`](/home/yuu/megu/src/structure_template.rs) の `place_blocks` + Processor 群 |
| **Kernel B: Jigsaw Pool + Template（10定義）** | `village_plains`, `village_desert`, `village_savanna`, `village_snowy`, `village_taiga`, `pillager_outpost`, `ancient_city`, `bastion_remnant`, `trail_ruins`, `trial_chambers` | `JigsawPlacement`（プール抽選・Joint 接続・AABB 衝突判定）+ Kernel A |
| **Kernel C: Procedural Piece Box（12定義）** | **完了(2)**: `mineshaft`, `mineshaft_mesa`<br>**単一Piece(4)**: `buried_treasure`, `swamp_hut`, `desert_pyramid`, `jungle_pyramid`<br>**複合Piece(6)**: `fortress`, `stronghold`, `end_city`, `monument`, `mansion` | [`src/mineshaft.rs`](/home/yuu/megu/src/mineshaft.rs) で確立した Piece BoundingBox クリップ + [`src/generation_payload.rs`](/home/yuu/megu/src/generation_payload.rs) |

### 3. Wave 実行計画（`gemini-4-hydrogen` 並列オーケストレーション）

- **Wave 1（進行中）: 構造的ブレーキの全撤去 & P1 100% 完結**
  1. `minecraft:end_spike` 実装 + Native Oracle 照合（Configured Feature 到達率 100% 達成）
  2. [`src/structure_template.rs`](/home/yuu/megu/src/structure_template.rs) の Fossil 専用制限の分離・全40種 `processor_list` 対応・テンプレート `LootTable` + BlockEntity ペイロード汎用化・近傍形状更新統合
  3. Seed 4242 Chunk `(3, -2)` の本番 `mineshaft` 接続後 **98,304 / 98,304 (0-diff)** 実証完了 & `POP_CHUNK` / `--pop-order` 自動化（**完了**）
- **Wave 2: 構造物バッチ前半（Kernel A 12定義 + 単一 Piece 4定義 + Jigsaw エンジン 10定義）**
  - Wave 1 で解放された [`src/structure_template.rs`](/home/yuu/megu/src/structure_template.rs) 上で並列実装。
- **Wave 3: 構造物バッチ後半（複合 Piece 5種） & 3次元サーバー配信 + M2/M3 ベンチマーク証明**
  - `fortress`, `stronghold`, `end_city`, `monument`, `mansion` の実装と、Overworld/Nether/End の runtime 統合・20TPS 負荷証明（[`specs/PLAN.md`](/home/yuu/megu/specs/PLAN.md) G1〜G4）。
