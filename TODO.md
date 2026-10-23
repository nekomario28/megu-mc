# TODO

2026-10-20 時点の実行可能な作業列。順序と出口条件は [ロードマップ](specs/ROADMAP.md)、
欠陥の状態は [close-defects](specs/close-defects/README.md) が持つ。ここは
「次に何を、何で判定するか」だけを置き、終わった項目は日付と結果を添えて消す。

## 進行中

- [x] **R1a 地形の ore vein 残差: 0 セル達成（2026-10-22）**
  - 1.21.1 の `OreVeinifier`（`dzi`）を公式 mappings + Vineflower で逆コンパイルして
    bytecode 読みを検証し、`Gen::vein_override` として実装（§29 の擬似コード）。
  - 最後の 1 点は乱数で、vanilla は `RandomState` の
    `fromHashOf("minecraft:ore").forkPositional()` を渡す（`randomState.e()`）。
    base factory を使っていたため 48/29/34 セルとぶれていた。
    `factory.of_str("minecraft:ore").splitter()` に直して **0 セル**。
  - 結果: (-3,3) 2×2 = **0**、8×8 spawn 周辺 = **4**（すべて tick する水）、
    cave only = **0**。
- [x] **R1e carver（洞窟・峡谷）: world 水準で 0 セル達成（2026-10-22）**
  - 原因は `minecraft:uniform` height provider の draw 数だった（§25）。
    1.21.1 は `eia` = `nextInt(max-min+1)` の 1 回。port は trapezoid `ehv`
    (plateau 1) の 2 回で組んでいたため、tunnel の yaw/pitch/yScale/count/seed が
    すべて 1 draw ずれていた。
  - サーバーの生ドロー（`dzx.c(JII)` の reseed + `dyz.c(I)` = `next(bits)`）と
    port の `CARVE_RAW` を突き合わせて特定。probe も `eia` に修正して一致。
  - 結果（2×2 at (0,0), y=-64..319）: cave 617→**0**、cave_extra 341→**0**、
    全 carver 617+341→**0**、canyon は jar の probability では start 無しで 0、
    probability 1.0 で **1 セル**。
  - 残り（§26 で分解済み）:
    - 8×8 の 52 セルのうち **48 は地形**（R1a の ore vein。§28 で正体判明:
      1.21.1 は `ore_veins_enabled` + router の vein 3 関数で**地形段階**に生成、
      リポジトリの 1.21.2+ データは `material_rule/copper_ore_vein.json`。
      chunk (‑3,3) で 48 vs 44、共通 42、bbox 一致 → 実装の境界差）
    - 4 セルは **worldgen ではなく水の流れ**（§27）。こちらの dump は隣に
      level 0 の水を持ち、vanilla の palette には level 1（流動）が居る。
      spawn 周辺は forceload で tick するため、サーバー稼働中に流体が広がる。
      canyon p=1 の 1 セルも同じ流動。**carver の world 比較は R1a を除いて 0**。
- [ ] **R1e-old carver（洞窟・峡谷）の実装**（実装済み・world 水準で残差）
  - 実装: `src/feature.rs` の carvers 節（`apply_carvers` / `carve_block` /
    `cave`+`room`+`tunnel`+`ellipsoid` / `canyon` / `CarvingMask`）と
    `worldgen.rs` の `carve_state` / `top_material_at` / `prelim_min`。
    feature より前に走る。記録: [carvers](evidence/gameplay/carvers/README.md) §5。
  - 検証: vanilla のクラスを直接呼ぶ probe（`tools/vanilla-probe/CarverProbe.java`）
    とセル単位で一致 — cave 1400、cave_extra 3042、canyon 342。制御 config の
    world 比較は 4 chunk で 1 セル差。
  - 残差（新しい固定具、49 chunk = (-5..1,-3..3)、seed 0、feature 無し）:
    vanilla の carver 由来 53601 セルに対し、一致 5289 / 掘り漏れ 48312 /
    掘りすぎ 41923。地形そのものは同じ固定具で 49 セル（R1a の ore vein）。
  - 切り分け済み（§6）: 残差は tunnel ではなく `carveBlock` の入力で、tag は jar と
    バイト一致。aquifer 連鎖（近接度・13 オフセット表・近い 3 つ・セル fluid 走査・
    レベル決定・kind 決定・センチネル・calc_density・barrier 3 テスト）は bytecode と
    一致を確認済み。**残る唯一の候補**は `a(IIILdyk$b;IZ)I` 冒頭の
    `dem.a(u, v, pos)` で、mc-spine は `excl` 1 本で代用している。
  - `dem.a` は erosion<-0.225 && depth>0.9 で、datapack の `aquifers.exclusion`
    (`min(-0.225-erosion, depth-0.9)`) と同値 → 一致を確認済み（§6）。
  - 形状は完全一致（chunk (1,1) の cave probe 401 = 401、セル集合も一致）。
    残差 1450 セルの内訳と我々の判断は §7。`AQ_TRACE` で見ると
    sim12<0 経路は最近 2 セルの level がセンチネル、sim12>0 経路は level が
    両方 63（= `|Δ|=0` で calc 0）。よって残るのはセル流体の level そのもの。
  - `prelim_est` は §8 で**一致を確認済み**（1.21.1 は `dzc.a(long)` が
    `initial_density_without_jaggedness > 0.390625` を 8 刻みで探す。datapack 側は
    同じ式 + (-0.390625) で同値）。§9 のとおり datapack 自体は 1.21.2+ の
    スキーマだが、依存値は同値。
  - §10: 固定具の chunk (1,1) では vanilla の実 world が cave 由来 0 セル
    （chunk 全体で 116）しか掘っておらず、probe の 401 セルは全て solid。
    同じ固定具の (-3,0) は 2870 セル。**拒否率が chunk 依存**で、これが残差の実体。
  - 次の一手: `NoiseChunk`(dzc) を組み立てて実物の `dyk$c` を作る probe を書き、
    `computeSubstance` をセル単位で比較する（carve 経路はブロック座標、
    terrain 経路は 4x8x4 格子座標で呼ばれる点と、`r[]`/`s[]` の共有状態が焦点）。
  - 併せて: `datapacks/vanilla` を 1.21.1 の jar から再生成する（現状は 1.21.2+ 形式）。
  - §11-13: hyperchunk (MIT) の `aquifer.c` と分岐単位で一致。PSL の開始 y と
    nearest-4 の同点規則を bytecode どおりに修正（数値は不変）。残差は
    chunk (1,1) cave 401 セル中、一致 250 / こちらが余分 151 / vanilla だけが
    water 116（この 116 は我々のどの carver セットにも無い別トンネル）。
    固定具の生成条件を疑う段階。
  - §15: datapack の `noise_settings` 上書きは既存 world に効かない
    （generator settings は level.dat にインライン）。no-aquifer 固定具は
    level.dat 再作成が必要。`aquifers_enabled` の読み取りは実装済み。
  - §25（解決）: `minecraft:uniform` height provider は 1.21.1 では `eia` で
    `nextInt(max-min+1)` の**1 ドロー**。port は `ehv`(trapezoid) の 2 ドローで
    組んでいたため draw が 1 つずれ、全トンネルが別物になっていた。
    修正後: cave only 2×2 = **0**、全 carver 2×2 = **0**。8×8 spawn 周辺に
    state 1↔2 の 52 セル残差（carver とは別件）。
  - §24: probe の DRAW 列を取得（`-Donly` 対応に修正）。source (1,-3) は
    x=4→20, y 156/140→84, z 2→-46。実サーバーは y=34, z=-41 なので
    **同じ setLargeFeatureSeed(0,1,-3) から2回目の y ドローで食い違う**。
    初期 seed を振っても probe は不変。次は `ayw` をサーバー側でプロキシして
    生ドローを記録し、probe と diff する（推測をやめる）。
  - §23: `ebb.carveTunnel` を計測（10,475 回）。source (1,-3) の最初のトンネルは
    実サーバー (20,34,-41) seed=1754441436904522573 / こちら (20,84,-46)
    seed=138348582335151166。**x だけ一致、y/z/seed/yaw が違う** → y サンプルで
    ストリームがずれている。bound は両者 236 で同じ。次は DRAW ログと
    `CARVE_RNG` を先頭 10 ドローで比較（プローブの bootstrap 修正が先）。
  - §22: `ehv`(UniformHeight) の inner 既定は 1 で、こちらの y サンプルと一致。
    §21 の「y が違う」は錯覚（y=84/122 のトンネルは空を掘るので ASTATE に出ない）。
    残る差は 3 本目 (start (23,33,-36)) が target (0,0) に届くか。yaw/pitch/count を
    エージェントで記録し CARVE_RNG と並べるのが次。
  - §21: §20 を訂正。no-fold / 隣 chunk read / ellipsoid bail-out 削除はいずれも
    617 のまま（効かない）。決定的なのは y の分布: 実サーバーの source (1,-3) は
    y=28..45 にしか彫っておらず（y>70 は 0）、こちらのトンネル開始は
    (20,84,-46)/(20,122,-36)/(23,33,-36)。**`cfg.y.sample` の結果が違う**。
    次は `above_bottom 8` の解決と uniform height provider の
    nextInt 引数/回数をサーバー内で実測する。
  - §20: エージェントに target を記録させると、target (0,0) で
    getCarveState を呼ぶ source は (1,-3) の 1 個だけ、書いたセルは 20。
    こちらの port は同じ start から 0 セル。原因は `carve_block` の `read` が
    `terrain_cell(cc, x, y, z)` を **x&15 で折り畳んで**評価するため、ターゲット外
    セルの地形判定が vanilla（隣 protochunk を読む）と違うこと。
    次の一手は「隣 chunk の ChunkCols で terrain_cell を評価する」＋書きは折り畳み。
  - §19: javaagent (`tools/carve-agent/`) で `applyCarvers` と `getCarveState` を
    サーバー内計測。target (1,1) の start 36 は我々と一致するが、**chunk (1,1)
    内の getCarveState 呼び出しは 0 件**。world は (1,1) を 1 セルも掘っていない。
    ずれは `carve` 内部（乱数消費/パラメータ）で、aquifer ではない。
  - §18: **§17 の結論を訂正**。cave p=1.0 の固定具で world 2353 セル /
    probe 5486 セル / 共通 212（9%）。seed 0..3 を振っても一致しない
    （最良 seed 1 で 27%）。p=1.0 では start の確率差が消えるので、
    **carve 段の呼び出しモデル自体が world と違う**。per-cell aquifer 比較は
    その修正後に行う。次はサーバー側 Mixin/javaagent で applyCarvers の
    seed/sourcePos/carverIndex/isStartChunk を実測する。
  - §17: probe に実地形を食わせると 401=401 で集合差 0 → 幾何・置換判定は完全一致。
    chunk (1,1) の差は「我々が余分に掘る」2 家族のみ（water/stone 152,
    air/stone 151）で、vanilla だけが掘るセルは 0。折り畳みガードも無実。
    残るのは carve 経路の aquifer 判定のみ。
  - §16: world の carve はモデルから再現できない（probe が 0 の chunk で 20 セル、
    probe が 401 の chunk で 0 セル）。次は level.dat 再作成で候補 2 を排除。
  - §14: 固定具を高速化（9 秒/サイクル、原点指定・region 削除・EXIT 復元）。
    **carver 全部 zero で地形は 0 セル差 = bit-exact**。carver 別では
    canyon 0 / cave 617 / extra 341。aquifer の 12 サンプル位置も Java 側の
    直接計算と完全一致。次は `CarverProbe` の fake chunk を実地形で埋めて
    「本物の aquifer 抜き oracle」にし、幾何か aquifer かを切り分ける。
  - 出口: `--keep-carvers` の vanilla 世界と block diff 0 セル（R1a の 49 セルを除く）。

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

## OSS survey (2026-10-21)

`evidence/foundation/oss-survey/README.md` に、ライセンス的に取り込める OSS の
一覧と判定をまとめた。要点:

- **Soju06/hyperchunk** (MIT, C11): 26.2 の overworld 全工程（noise/DF/SIMD,
  surface, carvers, aquifer, features, structures, lighting, spawn, NBT/region）を
  bit-exact 実装。`openspec/` が bytecode 再構成ノート。**aquifer.c は我々の
  `aq_state` と分岐単位で一致**（similarity/13 offsets/12 cell scan/calc/3 tests/
  skip formula/PSL memo/lava floor）。二つの独立実装が一致 → 残差は aquifer の
  外側（plumbing か fixture）。
- **MCHPRS** (MIT, Rust): redstone/tick。**cubiomes** (MIT): biome/structure。
  **chorus** (Apache-2.0): bedrock worldgen + lighting。**mc-datapack-map** (MIT):
  datapack の DF 評価の突き合わせ。**valence/azalea/PicoLimbo** (MIT): protocol。
- 使えない: Pumpkin (GPL-3.0), SteelMC (AGPL-3.0), Amidst (GPL-3.0),
  fluffy-parakeet (ライセンス無し)。

## feature 層の到達点（2026-10-22 追記）

feature あり 2×2 の diff は **40975 → 2062**（1 chunk あたり ~520 = 0.53%）。
効いた修正（いずれも公式 1.21.1 mappings + Vineflower で確認）:

1. `WorldgenRandom` は `dyn` の legacy draw（modern 形では ore origin が 242/242 ずれ）
2. height provider 4 種（`eia` uniform / `ehz` trapezoid plateau 0 / `ehv` biased /
   `eib` very_biased）と `below_top` の −1、`next_inbetween` の draw 抜け
3. `setDecorationSeed` = Xoroshiro の nextLong を **加算**合成（`(x*l + z*m) ^ seed`）。
   chunk (0,0) だけは 0 なので今まで露見しなかった
4. `underwater_magma` の前提（origin が水）と包囲判定
5. `placed_feature/` を 1.21.1 jar から取得（`trees_birch_and_oak` 欠落で木が全部スキップ）

残り: 木・disk は `feature/`（configured feature）が 1.21.2+ 形状のままで、
1.21.1 の値と噛み合わない。**configured_feature の 1.21.1 化 + パーサ両対応**が次の単位。
そのあと carver 層 1040 セル、構造物（未実装）、性能（R3）。

## R1a feature 層（2026-10-22 続き、現在 1856 セル）

feature あり 2×2 の diff: **40975 → 1856**（chunk (0,0) 単体 10177 → **140**）。
追加で直したもの:

6. **feature の並び順が依存する入力 biome 集合**: `FeatureSorter.buildFeaturesPerStep`
   は biome source の possibleBiomes（overworld **53 件**）を受け取る。64 件の registry 順
   （nether/end/the_void 込み）だと step 2/9/10 の添字がずれる。step 6（ore）が
   正しく見えていたのは「どの biome でも並びが同一」で tie-break に依存しないため。
   `trees_birch_and_oak` は 31 → **20**。
7. **placement modifier の落下**: `fixed_placement` 未対応で modifier が黙って落ちていた
   → `PF_UNKNOWN=1` を追加して実装。
8. **`would_survive` は設定された state を評価**する（現在のブロックではない）。
9. **disk 系が 0 書き込み**: 1.21.1 の rule-based state provider は `type` が無く、
   `"fluids": "minecraft:water"` は単一値 → どちらも解析失敗で feature ごと消えていた
   （`CF_MISSING=1` を追加して発見）。

残り: 葉の `distance` プロパティ（`oak_leaves != oak_leaves` 190）= TreeFeature の
`updateLeaves` 未実装、葉の位置 844、`diorite != water/gravel` 200、carver 層 1219。

計測器（すべてサーバー内 javaagent）: `FSEED` / `ORE` / `TREE` / `BIOMEF` / `INSQ` /
`RDRAW`（`dyn` の default draw）/ `BIOMELIST` / dimension ゲート（`I_()` = −64）。
port 側: `FSEED_TRACE` / `FSEED_ARM`（`NDRAW`/`FDRAW`/`XDRAW`）/ `ORE_TRACE` /
`TREE_TRACE` / `BIOMEF_TRACE` / `FEAT_TRACE` / `FEAT_LIST` / `PF_UNKNOWN` /
`CF_MISSING` / `MC_SPINE_BIOME_DUMP` / `MC_SPINE_HM_DUMP`。

## 葉の distance と樹冠（2026-10-22 追記）

- `LeavesBlock.updateShape` は state を変えず `scheduleTick` するだけ。保存済み world の
  `distance` は tick 連鎖が書いた値で、worldgen の出力ではない。実測: 保存済みの
  oak_leaves 590 のうち 166 (28%) が局所規則を満たさない（未収束）。
- `randomTick` は `!persistent && distance==7` の葉を消すので、forceload chunk は樹冠が欠ける。
- → `oak_leaves != oak_leaves`（distance のみ）は除外。ただし「こちらに葉・vanilla は air」
  の 578 は距離 1〜3 が大半で、幾何の差として残差に数える（次はここ）。

## 未解決: BiomeFilter の false（2026-10-22）

chunk (1,0) の候補 (20,66,3), feature (20,9) で vanilla の BiomeFilter は false、
こちらは true（biome=forest）、保存済み world の quart (20,64,0) は forest。
`BiomeFilter` は `level.getBiome(pos)` の biome の feature 集合を見るので、
forest に `trees_birch_and_oak` がある以上 false は説明できない。
次は `ctx.d().t(pos)`（`dcz.t(Ljd;)Ljm;`）の戻り値と `ctx.e()`（topFeature）を
文字列でログして、どちらが想定と違うかを見る。木は 45 vs 38。

## 残差の正体（2026-10-22 最終）

石系 ore のセル集合を vanilla と比較すると **vanilla-only 0 / ours-only 419** の
「上位集合」。差は step 8 の `spring_water`（等）が後から diorite を水に置き換える
分だけ。次の単位は spring 系（`SpringFeature` の rock_count/hole_count/valid_blocks）。

## 最終残差の分解（2026-10-22）

2×2 で 1035（既定の除外を入れると 655）。内訳:
- 葉の distance 380（tick 由来、既定で除外）
- vanilla の gravel/dirt/water vs こちらの ore ~379（ore_dirt/ore_gravel の欠け + spring）
- 樹冠の幾何 ~470（木 45 vs 38）
- carver/aquifer は現行コードで 0 セル（`carver_fixture.sh tcur - - - 2`）
