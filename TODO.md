# TODO

2026-10-20 時点の実行可能な作業列。順序と出口条件は [ロードマップ](specs/ROADMAP.md)、
欠陥の状態は [close-defects](specs/close-defects/README.md) が持つ。ここは
「次に何を、何で判定するか」だけを置き、終わった項目は日付と結果を添えて消す。

## 進行中

- [ ] **R1b surface rule の 194 セル**（y=54-55 の dirt↔grass）
  - vanilla `SurfaceSystem`（obf `dzq`）の `Context.updateY(..., waterHeight, ...)` へ
    渡る値を確定する。条件式 `dzp$z$a` は `waterHeight == MIN` → true、
    それ以外は `y + stoneDepthAbove >= waterHeight + offset + runDepth * mult` で一致済み。
  - 判定: feature/carver なしの `chunk_block_diff` が 194 → 0 付近。
    減らない／増える変更は撤回して数字を残す。

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
- [x] ローカル `depsgx` を固定 revision の venv に置き換え（同一バージョン表記の古い
      ビルドが cycle を 1 件しか報告しなかった）。

## 検証・公開

- [ ] `tests/verify.sh` を 2026-10-20 の変更で完走させ、ログを残す
      （hosted CI は支払いブロックのためローカルで代替）。
- [ ] `fix/20tps-integrated` を push し、hosted CI が復旧したら同一 SHA で再実行。
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
