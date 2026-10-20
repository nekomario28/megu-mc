# TODO

2026-10-20 時点の実行可能な作業列。順序と出口条件は [ロードマップ](specs/ROADMAP.md)、
欠陥の状態は [close-defects](specs/close-defects/README.md) が持つ。ここは
「次に何を、何で判定するか」だけを置き、終わった項目は日付と結果を添えて消す。

## 進行中

- [ ] **R3 チャンク生成の並列化**（M3 は 8 回中央値 x1.10、WARM=1 は x1.79 で未達）
  - 設計: [chunk-parallelism](specs/chunk-parallelism/README.md)（GenData/Gen の分離
    → ジョブ化 → worker >1、決定性テスト、`WARM=1` の前後測定）。
  - 現在: `populate9` 60 → 37 ms/chunk（地形読み 1797 → 295 ns/call、FC の列
    キャッシュを採用）。burst 込み 8 回の中央値 x1.10（RSS 142 対 ~1160 MiB）、
    burst を除く 3 回は x1.65-2.85。単発の x0.83 は主張しない。
  - チャンク単位の地形配列は**撤回**（構築 20.7 ms/chunk、移動視点で悪化）。
  - 残差は feature の単一スレッド実行（vanilla は 62-64 スレッド）。次は
    region/chunk pipeline の所有者を決め、読み取り専用の base terrain と
    チャンク単位で閉じた feature 書き込みを worker に渡して決定的にマージする。
  - 並列化までの間は `ore_dirt`（11.7 ms/chunk）と `monster_room(_deep)`
    （計 8.9 ms）のループ削減。測定は `WARM=1` を標準にする。
  - 判定: `load_compare.sh` の `WARM=1` 中央値。x1.10 未満は主張しない。

## 次

- [ ] **R1d の残り（任意）**: 区間マスクを vanilla と同じにする（1 chunk あたり
      約 94 KiB の節約）。GUI でクライアントの「マスクに無い区間」の扱いを
      確認しない方針のため、wire の値が一致した今は保留。
- [ ] **R1a 残り: 石系 ore の配置実行**（2026-10-20 に modifier 順を修正、11017 → 10302）
  - biome の feature リストは 1.21.1 に差し替え済み（10939 → 10874）。
    ore の configured/placed 定義は jar と同一。残差は andesite/diorite/granite/
    tuff/deepslate の**位置**で、modifier の RNG 消費・height provider・target 判定。
  - 次: vanilla の `PlacedFeature.placeWithContext` の bytecode を読み、
    位置ストリームの per-position draw 数を確定する。1 本目の vein は完全一致
    （原点 (7,49,13)、vanilla の 582 セル成分）だが 2 本目が (5,6,2) で一致しない。
    1 本目の後に 4 draw 余分とすると (1,20,8) になり vanilla の 263 セル成分と
    x/z が一致するが、chunk (1,0)/(0,1)/(2,0) では再現しないので単純な draw 差ではない。
    単一 ore だけの vanilla 世界（`tools/vanilla_ore_isolate.py`）では 1 本目が
    一致（原点 (7,49,13)、in-box 比 0.55）し、2 本目 (5,6,2) は 0.00 で存在しない。
    Java プローブで `OreFeature.place`（size 64）の draw が **67** と確定
    （`nextFloat` 1 + `nextInt(3)` 2 + `nextDouble`×64）。modifier も bytecode で確認
    （in_square は `nextInt(16)` ×2、count 定数は 0、height uniform は 1）。
    それでも 2 本目 (5,6,2) は隔離 vanilla 世界に存在しない。
    プローブに chunk 供給を足して書き込みセルを取得済み: seed 60007・(7,49,13)・
    size 64 で vanilla 666 セル = `ore_oracle.py` 666 セル（**完全一致**）。
    vein の幾何は確定。残りは位置だけで、次は `ejb`(CountPlacement) と
    `eis`(InSquarePlacement) の `getPositions` をプローブから直接呼び、
    位置2の位置と draw 数を vanilla から取ること。
  - 判定: 1 seed・1 chunk で andesite のセル集合が一致するか。
- [ ] **R1c feature step 別の差分表**（R1a の後）
  - `chunk_block_diff.py` に step 別の集計を足すか、populate の書き込みを step で
    タグ付けして数える。最初に直す 1 step を決める。
- [ ] **R1d 光の再測**（R1b の後）
  - `light_parity_probe.py` で区間別 digest。地形一致後に残る差を光源/伝播へ分ける。

## 完了（2026-10-20）

- [x] 地形（feature/carver なし）の差分を分離する固定具を作り、`vertical_gradient` の
      md5 読み出しを big-endian に修正。1221 → 194 セル。
      `tools/vanilla_nofeat_pack.py`、`evidence/gameplay/terrain-parity/README.md`。
- [x] 層ゲートの赤（生成系からの `crate::name_to_state`）と debt ratchet の赤
      （2 件目の ignored test）を解消。
- [x] surface rule の `not_underwater` 二重否定と waterHeight 近似を修正
      （(0,0) 194→5、(0,1) 243→36、(-1,-1) 228→0）。回帰テスト追加。
- [x] `interpolated` の入力をセル隅で補間し外側をブロック毎に評価
      （feature/carver なしの block diff が 4 chunk で 0）。回帰テスト追加。
- [x] 地形一致後の代表負荷を再測定（CPU x1.13 で M3 未達、RSS x8 軽い）。
      以前の x0.63 を現在の代表値として使わないことを close-defects に記録。
- [x] 生成費用の内訳を診断（feature 84%、ore_dirt 13 ms/chunk）。fill を 1 ロック・
      1 回計算に整理（代表負荷はノイズ内、パリティ 0 セル）。
- [x] feature の地形読みを 1797 → 295 ns/call（`FC` がチャンク列を保持）。
      `populate9` 667 → 411 ms、FEATPROF 1088 → 736 ms。capture digest 不変。
- [x] R1d: 地形一致後の光を 3 chunk で再測。heightmap と非一様 sky digest が
      vanilla と一致（mask の送り方だけ差）。
- [x] R1a スライス1: biome の feature リストを 1.21.1 の jar から差し替え
      （feature あり 10939 → 10874）。ore 定義は jar と同一と確認。
- [x] 石系 ore のずれを調査: RNG 演算は vanilla と完全一致、gi の総当たり
      （0..23）と全体シフト（-12..+12）でも vanilla の andesite に一致しない。
      biome の iteration 順の変更は悪化。残差は ore の実行側（未解決）。
- [x] M3: 8 回の burst 込み中央値 x1.10、`WARM=1` で x1.79。定常の feature 実行が
      律速と特定（`tools/load_compare.sh` の `WARM=1` を追加）。
- [x] LICENSE（MIT）と第三者データの境界を明記（#11 のソース側）。
- [x] ローカル `depsgx` を固定 revision の venv に置き換え（同一バージョン表記の古い
      ビルドが cycle を 1 件しか報告しなかった）。

## 検証・公開

- [x] 落下と並行操作のハードニング（[falling-concurrency](specs/falling-concurrency/README.md)）:
      性質テスト 5 件（保存中/破壊中/隣接破壊/変換中の保存/ホットセット変更）を
      `src/tick_tests.rs` に追加し、対応する変異 5 件で赤になることを確認。
      `cargo test` 147 passed / mutation 51 proved（2026-10-20）。
- [x] `tests/verify.sh` PASS（148 passed / 1 ignored、mutation 53 proved、8 threads × 3 回、
      log: `~/.cache/mc-spine-work/verify-2026-10-20-vein-golden.log`）。
      固定 revision の `depsgx` を `PATH` に置いて実行、ログは
      `~/.cache/mc-spine-work/verify-2026-10-20.log`。
- [x] `fix/20tps-integrated` を push（`568f40f` まで成功）。hosted CI は run
      39351452776 で **job が起動せず**（支払い/上限ブロック）。復旧後に再実行する。
- [ ] `8c15b6d` 以降（interpolated の順序、M3 訂正、生成費用の記録、fill の整理）は
      ローカルのみ。push は `gh auth status` が **token invalid** を返して失敗した。
      `gh auth login -h github.com` の後に `git push origin fix/20tps-integrated`。
- [ ] issue #7 の各項目を統合 head で再判定（spawn 周期、projectile drag、
      legacy counter 変換、負の drop age、save の副作用、malformed save、
      arrow collision の 0.9）。実行した証拠ごと close する。
- [ ] 草案 PR #2 / #4 / #6 を統合ブランチの内容と照合して閉じる。
- [ ] LICENSE を決めて追加（#11）。worldgen データの出自と再配布条件を明記。

## 保留・監視

- [ ] 密度値そのものの vanilla 比較（`interpolated` の順序を正しくした実装の再適用は
      そのとき）。現状は観測できる差が 4 セルで費用 +27% のため撤回済み。
- [ ] 生成器の並列化: [chunk-parallelism](specs/chunk-parallelism/README.md) の
      owner 決定（GenData 分離とマージ順）が前提。
- [ ] 代表負荷の再測定は R1 完了後。いまの x0.63 は地形が違う状態の値。
