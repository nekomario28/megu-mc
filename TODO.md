# TODO

2026-10-20 時点の実行可能な作業列。順序と出口条件は [ロードマップ](specs/ROADMAP.md)、
欠陥の状態は [close-defects](specs/close-defects/README.md) が持つ。ここは
「次に何を、何で判定するか」だけを置き、終わった項目は日付と結果を添えて消す。

## 進行中

- [ ] **R3 feature ロジック本体の削減**（M3 は x1.13 で未達）
  - 再測値: mc-spine 19.79s / 145.2MiB / 3thr 対 vanilla 17.59s / 1204.7MiB / 62thr。
    内訳: `chunk_cols` 11.5 ms/chunk、feature 約 60 ms/chunk（生成の 84%）。
    うち地形読みが 45%（1.8 µs/call、274k 回/25 chunk）、残りが feature ロジック。
  - チャンク単位の地形配列は試して**撤回**（構築 20.7 ms/chunk、移動視点で
    22.56s 対 19.79s に悪化）。地形の読みを作り直す方向では届かない。
  - 次: `FEAT_PROFILE` の上位（`ore_dirt` 13 ms、`monster_room(_deep)` 9.7 ms）を
    1 つ選び、候補セル数・書き込み数・地形読み以外の内訳を測って削る。
    並列化は所有者（region/chunk pipeline）を決めてから。
  - 判定: `FEAT_PROFILE` の該当 feature と `load_compare.sh` の CPU。x1.10 未満は
    主張しない。

## 次

- [ ] **R1a worldgen データを 1.21.1 に固定**
  - 1.21.1 server jar（SHA-256 を記録）から biome / configured_feature /
    placed_feature / noise_settings を取り、`datapacks/vanilla` の該当部分を置き換える。
    密度関数のファイル分割と `material_rule` は mc-spine 側の持ち物として残す。
  - 同梱 biome は 1.21.2+ 形式（`attributes` あり、`spawners` なし）。
    1.21.1 と feature リストが 48/64 不一致、type が 16 欠け・9 余る。
  - 判定: feature あり block diff（seed 0, chunk (0,0)）の前後。データ差の分が
    減ったことを確認し、減らなければ理由を特定する。
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
- [x] ローカル `depsgx` を固定 revision の venv に置き換え（同一バージョン表記の古い
      ビルドが cycle を 1 件しか報告しなかった）。

## 検証・公開

- [x] `tests/verify.sh` PASS（140 passed / 1 ignored、mutation 46 proved）。
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
- [ ] 落下の並行/順序ハードニング（close-defects から繰り越し）を専用スライスにする。

## 保留・監視

- [ ] 密度値そのものの vanilla 比較（`interpolated` の順序を正しくした実装の再適用は
      そのとき）。現状は観測できる差が 4 セルで費用 +27% のため撤回済み。
- [ ] 生成器の並列化。所有者（region/chunk pipeline）を決めるまで着手しない。
- [ ] 代表負荷の再測定は R1 完了後。いまの x0.63 は地形が違う状態の値。
