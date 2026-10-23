# ロードマップ

Status: 2026-10-22 更新。作業ブランチは `fix/20tps-integrated`（`main` より 259 コミット先、
`main` 側の clippy 3 コミットだけ未取り込み）。直近の実測値と却下した仮説は
[evidence/gameplay/terrain-parity](../evidence/gameplay/terrain-parity/README.md)、
目的と合格条件は [PLAN](PLAN.md)、欠陥の状態の正本は
[close-defects](close-defects/README.md) が持つ。この文書は**順序と出口条件**だけを持ち、
状態表を複製しない。

## 現在地

| 項目 | 実測・状態 |
|---|---|
| ビルド・テスト | `cargo build --locked` 成功。`cargo test` 152 passed / 1 ignored（2026-10-22） |
| 必須ゲート | `tests/verify.sh` PASS（152 passed / 1 ignored、local、2026-10-22） |
| hosted CI | GitHub の支払い/上限ブロックで job が起動しない（コード失敗ではない）。ローカル verify で代替し、復旧後に同一 SHA で再取得する |
| M3（軽量） | **未達**: 地形一致後の代表負荷で CPU x1.13（mc-spine 19.87s / 144.7MiB 対 vanilla 17.53s / 1159.7MiB）。以前の x0.63 は地形不一致時の値で、現在の代表値にしない |
| 地形（feature/carver なし） | vanilla と **0 セル**（(0,0) の 2×2）。density・surface rule・bedrock・**ore vein** が一致。高さマップも 256/256 列一致 |
| 洞窟・峡谷（carver） | vanilla と **0 セル**（(0,0) の 2×2、cave / cave_extra / canyon）。生ドローの一致まで確認（[carvers](../evidence/gameplay/carvers/README.md)） |
| 落下と並行操作 | 性質テスト 5 件・変異 5 件を追加（2026-10-20）。保存・破壊・設置・ホットセット変更が同じティック列で交差してもブロックを落とさない（[falling-concurrency](falling-concurrency/README.md)） |
| feature 層 | 2×2 で **1035 / 393216（0.26%）**。葉の `distance` 380 は tick 由来で、除外すると **655**。起点は 40975（10.4%/chunk、2026-10-22 朝） |
| 構造物 | **未実装**（jigsaw / template pool / structure_set / NBT のいずれも無い） |

## 順序

各段階は「測れる出口条件」を持つ。前段の出口条件が未達なら次段へ進まない。
チェックポイントは検証済みコミットで切り、`main` へは統合ブランチをそのまま入れない。

### R0 復元と基準（2026-10-20）

- 作業ツリーを `fix/20tps-integrated` に戻し、ビルド・テスト・verify のローカル基準を取る。
- 復元時に HEAD が必須ゲートで赤だった箇所を直した（いずれもこの日の変更ではない）:
  - `src/worldgen.rs` の生成系から `crate::name_to_state("bedrock")` を呼んでいた
    （a02cbf5 で混入）。層ゲートが root binding 越しの生成依存として拒否するため、
    `Gen` が load 時に bedrock state を解決して保持する形にした。
  - `#[ignore]` の診断ベンチが 2 件になり debt ratchet（上限 1）が赤だった。
    ベンチは `WGBEDROCK=1` の env ゲートへ移し、parked gate の数を 1 に戻した。
- ローカルの `depsgx` が同一バージョン表記（0.1.0）の古いビルドで、cycle を 1 件しか
  報告せず層ゲートの自己試験が落ちた。CI が固定する revision
  `d6b9ad8db4774919c8b4fb032342e53b2548b660` を venv に入れて `PATH` の先頭に置いた
  （`~/.cache/mc-spine-work/gxkit-venv`）。バージョン文字列ではなく revision で固定する。
- 出口: `tests/verify.sh` が PASS。ホスト CI の代替であることを記録する。

### R1 worldgen の 1.21.1 一致（最優先）

- **R1a データ版の固定（biome リスト完了 2026-10-20）**: 同梱 `datapacks/vanilla`
  の biome は 1.21.2+ 形式で、1.21.1 と feature リストが 48/64 不一致。64 biome の
  `features` を 1.21.1 の server jar から取り直し、feature あり block diff は
  10939 → **10874**（-0.6%）。トップの状態組は変わらず、残差は石系 ore の位置 =
  feature の**実行**側。configured/placed feature の ore 定義は jar と同一。
  証拠: [feature パリティ](../../evidence/gameplay/feature-parity/README.md)。
  残り: 石系 ore の modifier RNG / height provider / target 判定を vanilla の
  bytecode と突き合わせる。全 type を 1.21.1 に揃えるのは mc-spine が新しい型
  （`vegetation_patch` 等）で書かれているため、型の実装追加が前提。
- **R1b surface rule の残差（完了 2026-10-20）**: `not_underwater` の二重否定と
  waterHeight の近似を直し、(0,0) 194→5、(0,1) 243→36、(-1,-1) 228→0。
  残りは 1 ブロックの表面段差で density の境界。
- **R1b2 density の補間順序（完了 2026-10-20）**: `interpolated` の入力をセル隅で
  補間し、外側をブロック毎に評価。feature/carver なしの block diff は 4 chunk で 0。
  `chunk_cols` は 48.8 → 70.8 ms/chunk なので、R3 で代表負荷を測り直す。
- **R1c feature 層の切り分け（大枠は完了 2026-10-22）**: feature あり 2×2 の diff は
  40975 → **1035**（除外規則込みで 655）。step 別の寄与と原因は
  [carvers §25〜§42](../evidence/gameplay/carvers/README.md) に記録した。判明した原因は
  いずれも「宣言的なものが黙って落ちる」型:
  `minecraft:uniform` の draw 数（1 vs 2）、`WorldgenRandom` の legacy draw、
  height provider 4 種と `below_top`、`setDecorationSeed` の合成（Xoroshiro + 加算）、
  feature 順序の入力 biome 集合（**53 件**であって 64 件ではない）、
  placement modifier の落下（`fixed_placement`）、
  rule-based state provider の `type` 欠落と `"fluids"` の単一値、
  入れ子 placed feature 参照の modifier 落ち。
  残りの出口: **B 分類**（vanilla にあってこちらに無い gravel/dirt/water ~379 =
  `ore_dirt`/`ore_gravel` の欠け + spring の水）、**樹冠の幾何** ~470（木 45 vs 38）。
  未解決: chunk (1,0) の候補 (20,66,3) で vanilla の `BiomeFilter` が false を返す
  （保存済み biome は forest）— `ctx.d().t(pos)` の戻り値と `ctx.e()` を実測する。
- **R1d 光（完了 2026-10-20）**: 地形一致後に `light_parity_probe.py` を 3 chunk で
  実行。heightmap と非一様区間の sky digest が vanilla と一致した。差は区間マスクの
  送り方（全 26 区間 vs 一様区間の省略）だけで、値ではない。

### R2 M2「遊べる」の一致

- CLI（`load_client.py` / `gameplay_wire.py`）で、移動・破壊設置・所持品・クラフト・
  精錬・収納・明暗・昼夜・天候・戦闘・死亡/リスポーン・基本 mob のセッションを
  同一入力で vanilla と比べる。実画面は使わない（ユーザー指示）。
- 出口: 各項目に wire の assert と vanilla 対照。未実装機能は「未実装」と明記し、
  近似を一致として数えない。

### R3 M3「軽量の証明」（地形一致後の再測で未達、削減が次のスライス）

- 地形一致後に測り直した: 8 回の 20 秒窓で中央値 **x1.10**（mc-spine 17.96s /
  142MiB / 3thr 対 vanilla 16.33s / ~1160MiB）。spawn burst を除く `WARM=1` は
  **x1.65-2.85（中央値 x1.79）**。以前の x0.63 は地形不一致時の値。
- 内訳（release）: `chunk_cols` 11.5 ms/chunk、feature の populate 約 60 ms/chunk。
  **生成時間の約 84% は feature**で、上位は `ore_dirt` 13 ms/chunk、
  `monster_room(_deep)` 9.7 ms、`ore_gravel` 3.9 ms など。populate の 45% は
  地形読み（1.8 µs/call）、残りは feature ロジック。証拠:
  [生成器の費用](../../evidence/gameplay/worldgen-cost/README.md)。
- チャンク単位の地形配列は試して撤回（構築 20.7 ms/chunk、移動視点で悪化）。
  feature 側の列取得をチャンク単位にキャッシュして読みを 1797 → 295 ns/call、
  `populate9` を 667 → 411 ms にした。それでも M3 は未達。残差は feature の
  単一スレッド実行なので、次のスライスは **チャンク生成の並列化**
  （owner: region/chunk pipeline）。並列化までは `ore_dirt`/`monster_room` の
  ループ削減。測定は `WARM=1` を標準にする。
- 出口: 同一 seed・同一負荷で CPU を再測定し、x1.10 未満の差は主張しない。

### R4 統合・公開

- `fix/20tps-integrated` をレビュー済みのスライスに分けて `main` へ入れる。
  草案 PR #2/#4/#6 は統合ブランチに含まれるので、統合の事実を確認して閉じる。
- issue #7（time-contract レビュー）の各項目を統合 head に対して再判定し、
  実行した証拠とともに閉じる。issue #1/#3 の残項目も同じ。
- LICENSE を決めて追加（#11）。worldgen データの出自（Mojang 資産）と
  再配布条件を README/SECURITY_NOTES に明記する。
- 出口: `main` の CI が緑（支払い復旧後）、未修正項目が空、独立レビュー済み SHA。

### R5 Mod 互換の拡張

- [mod-compatibility](mod-compatibility/README.md) のスライス 04/05。
  状態を持つ block entity・inventory・tick callback を 1 mod ずつ契約化する。
- 出口: 対象 mod の実 JAR で、通常経路と保存/再読込の一致。

## 横断的な規律

- 欠陥の状態は close-defects に集約し、この文書へ複製しない。完了は
  「実装・実行した検証・独立レビュー・未実行ゲートなし」が揃ったときだけ。
- 計測は同一ビルド・同一条件で前後を取る。別条件の数字を比較しない
  （`chunk_cols` を 9.3ms と比べて 7 倍と誤認しかけた。同一 `WGPERF` では
  50.9 → 64.4ms だった）。
- 効果のない変更は撤回し、撤回した事実と数字を evidence に残す。
- 未実行のゲートは成功ではない。`tests/verify.sh` の終了値で停止する。
