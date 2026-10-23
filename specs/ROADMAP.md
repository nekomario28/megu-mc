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
| feature 層 | 2×2 で **704 / 393216（0.18%）**。葉の `distance` は tick 由来で、除外すると **285**。木は 39 本（vanilla 38）。起点は 40975（10.4%/chunk、2026-10-22 朝） |
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
  13. **feature 層のハイトマップの所有者は chunk**: 書き込まれた位置を所有する chunk の
  ハイトマップを `setBlock` ごとに更新する（`OCEAN_FLOOR` は樹冠の葉も数える）ので、
  隣の chunk の木がこちらの候補の y を上げる。1 回の populate 内で数えていたため
  候補 (0,17) が y=64 になり、vanilla が棄却する木を 1 本余分に作って以降のドローが
  全部ずれていた（940 → **704**、木 42 → 39）。
  14. **biome は 2 種類ある**: chunk container（quart セル、保存 world の中身）と
  `BiomeManager.getBiome(BlockPos)`（8 近傍を zoom seed の重み付き距離で選ぶ blend）。
  `LevelReader.getBiome` は後者で、`BiomeFilter` はそれを使う。前者を読んでいたため
  river/forest 境界で候補を誤って受理していた（実装後 1035 → **940**、木 45 → 42）。
  14. **feature を走らせる条件**: vanilla は装飾対象 chunk の **3×3 の section palette を
  union** して feature を選ぶ。こちらは 3×3 の 9 マスを y=0 でサンプルしていたため
  **cave biome を読み地表 biome（river）を落とし**、chunk (-2,1) で `trees_water` ほか
  3 件を実行していなかった（step 9 が 9 件 vs vanilla 12 件 → 修正で一致）。
  15. **装飾順は「順序」ではなく並列進行度**: `applyBiomeDecoration` の入口を計装すると
  worker が混在（x ごとに z が昇順でない）＝ vanilla は**並列に装飾**する。chunk status の
  依存（FEATURES の前提は隣が CARVERS）だけでは説明できず、実測では**隣の並列書き込みを
  見ている**（「隣を見ない」モデルにすると chunk (0,0) が 82 → 1924 セルに悪化。
  ore の探索箱ゲートが閉じるため）。よって固定具 world の厳密な再現には並列進行度の
  モデル化が要る。現行の「3×3 を先に decorate」はその最良近似（82 セル）。
  残差 704 は「並列依存の部分（ore 系ゲートと隣 origin の書き込み）」と「依存しない部分
  （樹冠の形・spring の水・disk）」に分かれ、後者が本物の実装差として残る。
  隔離世界（ore/disk を 0 にした world）での**ブロック単位比較**が有効:
  幹は `oak_log` 50/50・`birch_log` 12/12 で**完全一致**、葉だけがずれる
  （`birch_leaves` は 21 セル欠け、x=30..31 / y=68..71 / z=0..4）。
  4×4 の隔離固定具では `birch_leaves` が **471 = 471 で完全一致**（クラスタも同一）
  ＝ 2×2 の 21 セル欠けは**その world の装飾スケジュール差**で、樹冠の形ではなかった。
  実施済み: `tools/world_diff.py` で参照を2回生成して比較 → **床 22 セル / 8 chunk**
  （1 chunk の seagrass パッチに集中）。ただし後続の4×4隔離ダンプ（`/tmp/fl.bin` と
  `/tmp/orefix-rp/world`）は `carver_diff.py` で **42654 差**、内訳 deep 41394 / carver 1260、
  比較可能な8 chunk・skip 8 chunk だった。これは以前の記録の「938 は葉438 / gravel225 /
  植生173」や「4×4総 diff は feature 残差」と一致しない。fixture の段階・world・skip条件を
  再確認するまで、この総数から feature の原因を推定しない。vanilla 実行間の床22も、この
  新しい比較対象へそのまま一般化しない。
  16. **configured-feature の型カバレッジ**: datapack 内60種、`p_cf` は受理名を実装する
  （alias を含むため単純な割り算はしない）。未対応型は `Unknown` として空振りしうる。
  `kelp` の bytecode 準拠実装を追加したが、現fixtureの両側とも0セルで parity 未検証。
  次は kelp が実在する海洋 biome の完全生成 reference fixture を作り、typed draw と
  cell sets を比較する。その後に次の未対応型へ進む。
  **出口条件への含意は仮説**: chunk 装飾の順序/可視性が残差に影響する証拠はあるが、現在の
  4×4出力は deep/carver に分類され、feature残差の根拠にならない。現固定具で block diff 0
  が原理的に不可能とも、protocol client が唯一の方法ともまだ言えない。fixture の段階と
  比較対象が揃った後に改めて判定する。
  16. **装飾順は chunk ごとに違う**: 現行の「3×3 を先に decorate」は固定具の world
  （spawn 準備で周辺が先に装飾される）と整合する。一律に「自分の chunk だけ」へ変えると
  マテリアルルール（feature ハイトマップを読む）まで動き、704 → **6709** に悪化する。
  順序に依存しない固定具（spawn から離れた chunk を自力生成）は未達で、
  `forceload` は spawn 近傍以外で失敗し `setworldspawn` は再準備しない。
  道は (a) 自前 protocol client で遠方を歩かせて順序を作る、(b) level.dat に spawn を
  書いてから新規作成、(c) spawn 準備の spiral 順を再現、(d) 順序依存セルを除外。
  残りの出口: **B 分類**（vanilla にあってこちらに無い gravel/dirt/water =
  `ore_dirt`/`ore_gravel` の欠け + spring の水）、**樹冠の幾何**（木 39 vs 38）。加えて
  **装飾順の差**: seed も添字も一致するのに chunk (-2,1) の木が一致しないのは、
  feature 実行時に「どこまで装飾済みか」が違うため（vanilla は先に装飾した chunk の
  書き込みを全部見る／こちらは 3×3 をオンデマンドに populate するので順序が別）。
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
