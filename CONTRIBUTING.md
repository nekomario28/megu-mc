# Contributing / 貢献について

mc-spine は実験段階のサーバーで、完成品ではありません。小さく検証できる変更を歓迎します。
このページは**作業の進め方**と**提出時の契約**を持ちます。環境・データ・テストの詳細は
[開発ガイド](docs/development.md)、現在の作業は [specs/README.md](specs/README.md)、
次の順序は [specs/ROADMAP.md](specs/ROADMAP.md) を参照してください。

## 最初の一歩

1. [specs/README.md](specs/README.md) で、いま何が未確認かを読む。
2. [specs/ROADMAP.md](specs/ROADMAP.md) で、次に閉じる出口を確認する。
3. [docs/development.md](docs/development.md) で、環境・データ・テストの段階を確認する。
4. 変更するownerを1つに絞る。`rg` でシンボルと呼び出し元を確認し、既存のテストを読む。
5. 実行したコマンドとexit、実行しなかった段階を控える。提出時にそのまま書く。

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

- コミットは `git commit -s` で `Signed-off-by:` を付ける（[DCO 1.1](https://developercertificate.org/)）。CIではまだ強制していません。
- AI支援ツールの attribution 行（`Generated with ...`、`Co-Authored-By: <agent>` など）を入れない。
- 生成データ・`*.jar`・`*.class`・マッピング・`world_overlay.bin`・秘密情報をコミットしない。
- 共有worktreeで衝突したら、その場で解決するか `git merge --abort`。conflict markerを残さない。

## データと来歴

**GPL／AGPL／LGPLのコード・データを写さない**という方針は維持します。Pumpkin（GPL-3.0）からの機械的な書き換えも含めません。
アルゴリズムはvanillaの挙動・公開された仕様・論文から書き、参考にした公開情報をPRに書きます。トークン一致の検査は
`tools/provenance_scan.py`、記録は [THIRD_PARTY.md](THIRD_PARTY.md) §8です。

- Mojang由来のデータ（jar・マッピング・`datapacks/vanilla/`・`config_packets.bin`・javap出力・生成JSON）を**新たに追加しない**。
  必要なデータは手元のjarから [開発ガイド](docs/development.md#22-生成器) の手順で生成し、コミットしない。
- `--accept-eula` はMinecraft EULAへの同意です。自分が同意するときだけ使い、他人・CIの代わりに付けない。
- NeoForge／Fabric Loader／Mixinなどのjarとソースをコミットしない。`compat/*/dependencies.json` のURLとSHA-256で取得する。
- 第三者のコードを入れるときはライセンスを確認し、[THIRD_PARTY.md](THIRD_PARTY.md) と [NOTICE](NOTICE) に追記する。
  許可リスト（`deny.toml`）にGPL系を足さない。

### 現行ツリーの未解決事項

このHEADには、上の方針と**食い違う追跡データ**があります。分類・パス・確認した事実は
[開発ガイド](docs/development.md#24-追跡データと方針文書の食い違い未解決) にまとめました。要点だけ:

- Mojangのjar由来の生成データ（`datapacks/vanilla/`、`config_packets.bin`、`tests/data/` のfixture、`assets/*_1211.bin` など）が追跡されています。
- `assets/pblocks.json` / `assets/properties.json` は、[THIRD_PARTY.md](THIRD_PARTY.md) §3 がPumpkinのファイルとバイト一致と記録したsha256のまま追跡されています。
- 出所不明の `resourcepack.zip` や `datapacks/{modtest,rectest,spinetest,tagtest,testpack}/` も追跡されています。

**貢献者へのお願い**: 追加しない、削除しない、公開しない。判断はオーナーと [LICENSING.md](LICENSING.md) §5／
[THIRD_PARTY.md](THIRD_PARTY.md) §3・§9 に委ねます。食い違いを見つけたら、変更を混ぜずにissueで
「パス・分類・確認したコマンド」を報告してください。ここでライセンスを決めないでください。

## ライセンス

- **inbound = outbound**: 提出した変更（PR・パッチ）は Apache-2.0（[LICENSE](LICENSE) §5）でmc-spineにライセンスされるものとします。
  別の条件を付ける場合は提出前に明記してください。自分が書いた、または再ライセンスする権利のあるコードだけを提出してください。
- Apache-2.0 は**提案段階**です。[LICENSING.md](LICENSING.md) §5 の項目が解消するまで確定せず、公開・リリース可能でもありません。
  「公開してよい」と受け取れる書き方をしないでください。
- ライセンスの状況と未解決項目は [LICENSING.md](LICENSING.md)、ファイルごとの出所は [THIRD_PARTY.md](THIRD_PARTY.md)。
- 秘密情報（`.env`、鍵、トークン）をコミットしない。見つけたら [SECURITY.md](SECURITY.md) へ。

## English summary

Contributions are licensed under Apache-2.0 (inbound = outbound), and commits should be signed off (DCO).
Do not copy GPL/AGPL/LGPL code or data (including Pumpkin). Do not add Mojang-derived data, mappings or
third-party jars. Record any third-party material in THIRD_PARTY.md/NOTICE. This HEAD tracks some data that the
policy documents say is generated locally; do not add to it, delete it, or publish it — report it and leave the
decision to the owner. The repository is not publish-ready. See docs/development.md for environment, data and
test tiers.
