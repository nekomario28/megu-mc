# Contributing / 貢献について

mc-spine は実験段階のサーバーで、完成品ではありません。小さく検証できる変更を歓迎します。
このページは**作業の進め方**と**提出時の契約**を持ちます。環境・データ・テストの詳細は
[開発ガイド](docs/development.md)、現在の作業は [specs/README.md](specs/README.md)、
次の順序は [specs/ROADMAP.md](specs/ROADMAP.md) を参照してください。

## 最初の一歩（3 コマンド・オンボーディング）

Clone 後にまず以下の 3 コマンドで環境とゲートを確認してください：

```sh
# 1. ツールチェーン (Rust 1.98.0, Python 3, Java 21) と入力データの確認
python3 tools/dev_check.py

# 2. モジュール境界・技術的負債・来歴（Provenance）の静的ゲート
bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py

# 3. ターゲットを絞った高速テスト確認
cargo test --locked <test_filter>
```

（クリーンエクスポートブランチから作業する場合は `bash tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/1.21.1-server.jar --fetch-fabric-mappings --accept-eula` で vanilla データを生成できます。）

1. [specs/README.md](specs/README.md) で、いま何が未確認かを読む。
2. [specs/ROADMAP.md](specs/ROADMAP.md) で、次に閉じる出口を確認する。
3. [docs/development.md](docs/development.md) で、環境・データ・テストの段階を確認する。
4. 変更する owner（モジュール）を 1 つに絞る。`rg` でシンボルと呼び出し元を確認し、既存のテストを読む。
5. 実行したコマンドと exit、実行しなかった段階を控える。提出時にそのまま書く。

## 並列開発のモジュール分担（コンフリクト防止ルール）

複数人で並行作業する際は、以下の 3 トラックに作業ブランチを分け、`src/main.rs` での衝突を避けてください：

- **Track 1（Worldgen & Structure Parity）**: `src/worldgen.rs`, `src/feature.rs`, `src/structure_*.rs`, `src/jigsaw_structures.rs`, `src/template_structures.rs`, `src/single_piece_structures.rs`, `src/end_city_mansion.rs`, `tools/structure_capture.py`
- **Track 2（NeoForge / Fabric Mod 互換レイヤ）**: `compat/neoforge-events/`, `compat/fabric-probe/`, `src/java_compat.rs`, `src/landing*.rs`, `tools/shimcheck.py`
- **Track 3（Runtime / Protocol 767 / Gameplay / Networking）**: `src/main.rs`, `src/entity_*.rs`, `src/lighting.rs`, `src/paletted.rs`, `tests/*_wire.py`
  - **`src/main.rs` の Single-Writer ルール**: `src/main.rs` を編集するブランチは同時に 1 つに絞るか、新しいロジックは専用サブモジュールに切り出して `src/main.rs` 側は最小の呼び出し配線のみにしてください。

作業中のブランチやworktreeを上書きしないでください。`git add -A` / `git commit -a` / `git stash` で
他人のindexを巻き込まず、pathspecで自分の変更だけをstageします。

## 変更の進め方

- **1変更1責務**。無関係な整形・改名・移動を混ぜない。
- **実装とテストを同じ変更に入れる**。境界条件は、既存テストの隣に1ケース足す。
- **速い段階から確認する**。[開発ガイド](docs/development.md#4-テストの段階) のT0（生成データ・サーバー不要）→ T1（cargoのunit）→ T2（`tests/verify.sh`）の順。
- **緑の意味を広げない**。T0やT1の成功は、`tests/verify.sh` 全体・native照合・hosted CI・M2/M3の代用ではない。
- **数値が動かない変更を「効果なし」と読まない**。新しい経路が実行されたことを、カウンタ・trace・意図的な失敗のいずれかで先に示す（[DESIGN.md](DESIGN.md#比較の約束)）。
- **生成stageと入力順を変えない**。worldgenの比較は、seed・datapack・structures・source順・stageを固定して初めて成立する。

提出物には次を書いてください。

- 変更したowner（ファイル）と、なぜそこか。
- 実行したコマンド、exit、所要時間。実行していないコマンドは依存（jar、EULA、`depsgx`、時間）とともに「未実行」と書く。
- 追加・更新したテスト名と、そのテストが失敗しうることの確認（mutationや意図的な破壊）。
- 残る限界。fixture 1件の一致を全seedへ広げない。

## コミット

- コミットは必ず `git commit -s` で `Signed-off-by:` を付ける（[DCO 1.1](https://developercertificate.org/)）。
- AI支援ツールの attribution 行（`Generated with ...`、`Co-Authored-By: <agent>` など）を入れない。
- 生成データ・`*.jar`・`*.class`・マッピング・`world_overlay.bin`・秘密情報をコミットしない。
- 共有worktreeで衝突したら、その場で解決するか `git merge --abort`。conflict markerを残さない。

## データと来歴

- **C2ME (`MIT`) や Lithium (`LGPL-3.0`) などの互換ライセンス実装との関係**:
  - `mc-spine` 本体は `GPL-3.0-or-later`（寄稿は `LGPL-3.0-or-later` とのデュアルライセンス）であるため、**C2ME (`MIT`)** や **Lithium (`LGPL-3.0`)**、**Brigadier (`MIT`)** などの `MIT` / `Apache-2.0` / `BSD` / `LGPL-2.1` / `LGPL-3.0` プロジェクトは両方と 100% ライセンス互換です。
  - もし他プロジェクト（C2ME / Lithium 等）からアルゴリズムやコードを取り込む場合は、①クリーンルームで仕様から実装する、または②取り込む場合は必ず上流の著作権表示とライセンス（`MIT` / `LGPL-3.0` 等）を [THIRD_PARTY.md](THIRD_PARTY.md) と [NOTICE](NOTICE) に明記し、後で本体を `LGPL-3.0-or-later` としても扱えるように **GPL-3.0 専用 (`GPL-3.0-only`) の第三者コードだけは直接コピーしない** ようにします。
- Mojang由来のデータ（jar・マッピング・`datapacks/vanilla/`・`config_packets.bin`・javap出力・生成JSON）を**新たに追加しない**。
  必要なデータは手元のjarから [開発ガイド](docs/development.md#22-生成器) の手順で生成し、コミットしない。
- `--accept-eula` はMinecraft EULAへの同意です。自分が同意するときだけ使い、他人・CIの代わりに付けない。
- NeoForge／Fabric Loader／Mixinなどのjarとソースをコミットしない。`compat/*/dependencies.json` のURLとSHA-256で取得する。
- 旧 `assets/pblocks.json` / `assets/properties.json` および `resourcepack.zip` は削除済みで、`tools/provenance_check.py` で再混入を禁止しています。

## ライセンス

- **本体ライセンス**: `GPL-3.0-or-later`（[LICENSE](LICENSE)）。
- **寄稿コードは `LGPL-3.0-or-later` および `GPL-3.0-or-later` のデュアルライセンスで提供されるものとする。**（[LICENSE](LICENSE) / [COPYING.LESSER](COPYING.LESSER)）
- 自分が書いた、または上記条件でライセンスする権利のあるコードだけを提出してください。
- ライセンスの技術的整理は [LICENSING.md](LICENSING.md)、ファイルごとの出所は [THIRD_PARTY.md](THIRD_PARTY.md)。
- 秘密情報（`.env`、鍵、トークン）をコミットしない。見つけたら [SECURITY.md](SECURITY.md) へ。

## English summary

The project is licensed under `GPL-3.0-or-later` ([LICENSE](LICENSE)), and all contributed code is provided under a dual license of `LGPL-3.0-or-later` and `GPL-3.0-or-later` ([COPYING.LESSER](COPYING.LESSER) and [LICENSE](LICENSE)). Commits must be signed off (`git commit -s`, DCO 1.1).
When referencing or adapting permissive or LGPL-3.0 projects (such as C2ME under MIT or Lithium under LGPL-3.0), record attribution in [THIRD_PARTY.md](THIRD_PARTY.md) and [NOTICE](NOTICE), and avoid copying third-party `GPL-3.0`-only code so that `LGPL-3.0-or-later` compatibility is preserved. Do not add Mojang-derived data, mappings, or third-party jars. Run `python3 tools/dev_check.py`, `bash tools/layers_check.sh`, `bash tools/debt_ratchet.sh`, and `python3 tools/provenance_check.py` before opening a PR.
