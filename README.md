# mc-spine

Minecraft Java 1.21.1（protocol 767）向けの実験的なRustサーバーです。小規模・低負荷の世界で、観測できる挙動を保ちながら計算量を減らし、必要な既存Modとの接続を試しています。**Minecraft全体との互換性や、vanillaより軽いことが確認済みの製品ではありません。**

## セットアップ（最初に1回）

Mojang由来のデータ（worldgenのJSON、ブロック状態、biome木、クライアントへ送る registry/tag パケット）はこのリポジトリに含まれません。**手元の Minecraft 1.21.1 サーバー jar から生成します**（公式の bundler `server.jar`、sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`。jar は配布していません）。Java 21 と python3 が必要で、約20秒です。

```sh
tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings --accept-eula
```

- `--fetch-fabric-mappings`: 難読名の解決に使う Fabric の intermediary / Yarn マッピング (CC0, sha256 固定) の2ファイルだけをダウンロードします。手元にある場合は `--mappings DIR` を使います。それ以外は jar だけで動き、ネットワークは使いません。
- `--accept-eula`: `config_packets.bin` を作るため、jar から実サーバーを一時ディレクトリで1回起動します（オフライン・ループバックのみ）。Minecraft の EULA (https://aka.ms/MinecraftEULA) への同意が必要で、このフラグが同意の意思表示になります。付けなくても他のデータは生成されますが、クライアントは registry を受け取れません。

生成物（すべて `.gitignore` 済みでコミットされません）: `datapacks/vanilla/`、`assets/block_states_1211.json`、`tests/data/biome_probe_1211.tsv`、`config_packets.bin`。`resourcepack.zip` は任意で、自分のリソースパックを置いたときだけ配信されます（置かなければ配信しません）。

## 試す

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
MC_SPINE_PORT=25699 cargo run --locked
```

設定とdatapackはリポジトリ直下から読みます。通常のゲームlistenerは `127.0.0.1` にbindします。外部ネットワークに公開しないでください。認証・権限・保存の注意は [SECURITY_NOTES.md](SECURITY_NOTES.md) を参照してください。

現在の検証範囲と未解決事項は [プロジェクト状態](specs/README.md) にあります。テスト成功は、vanillaとの全機能一致や性能優位を意味しません。

## 確認できている範囲

| 領域 | 確認範囲 |
|---|---|
| worldgen | 地形とcarverは限定した固定具で照合済み。feature層と構造物は未完了 |
| ゲーム操作 | CLI経由の通信・操作を検査。全ゲーム要素の一致は未完了 |
| Java Mod | 特定のSeed Protect契約をFabric/NeoForgeで検査。任意Mod対応ではない |
| 軽量性 | 目標未達。比較条件と最新結果は[費用の証拠](evidence/gameplay/worldgen-cost/README.md)を参照 |

例として、feature parity の作業では `cargo test --release` の結果に加え、vanilla側のブロックセル集合と参照chunkの生成状態を比較します。最新の実行結果と、その比較で証明できていない点は[状態ページ](specs/README.md)に記録します。

## 開発と設計

- [目的と設計原則](DESIGN.md)
- [目標・検証契約](specs/PLAN.md)
- [実装順と出口条件](specs/ROADMAP.md)
- [診断・固定具の道具](tools/README.md)
- [worldgenの証跡](evidence/gameplay/)
- [Fabric契約](compat/fabric-probe/README.md) / [NeoForgeイベント契約](compat/neoforge-events/README.md)

Cargoのpackage version `0.0.0` は復元時の値です。`AUDIT_WIRE.md` と `SECURITY_NOTES.md` は監査記録であり、現在の安全性や完成度を保証しません。

## ライセンスと第三者データ

コードは Apache-2.0（[LICENSE](LICENSE)、[NOTICE](NOTICE)）です。ただし**この選択は提案段階で、公開前の確認事項が残っています**（[LICENSING.md](LICENSING.md)：Pumpkin(GPL-3.0)との一致の残り、テスト値表、履歴に残る旧データなど。解消できない場合は GPL になる可能性があります）。貢献の条件は [CONTRIBUTING.md](CONTRIBUTING.md)。NeoForge / Fabric Loader / Mixin / Mojang のデータは**同梱も再配布もせず**、利用者の環境で取得・生成します（それぞれ元のライセンスのままで、mc-spine の Apache-2.0 の対象外）。Mojang由来のデータ（`datapacks/vanilla/`、`assets/block_states_1211.json`、`config_packets.bin`）はリポジトリに含めず、利用者が手元の jar から生成します（上記セットアップ）。生成物は Apache-2.0 の対象外です。詳細は[復元と検証の記録](RESTORATION.md)と[第三者データの注意](SECURITY_NOTES.md)を参照してください。Pumpkin (GPL-3.0, commit `b0c48b2`) のソースとの12トークン一致を調べる検査を行い、一致していた範囲のうち書き直せるものは vanilla の仕様・挙動から書き直しました。書き直していない範囲 (6件)、一致するテスト値は残っています。Pumpkin とバイト一致していたデータファイルは削除し jar からの生成に置き換えました (§9)。ただし git 履歴には旧ファイルが残っています（[THIRD_PARTY.md](THIRD_PARTY.md) §8）。これは調査結果の事実の記述であり、ライセンス上の判断ではありません。

各ファイルの出所の一覧と、オーナー確認待ちの項目(Rust 依存のライセンス、履歴に残る旧データ、`assets/blocks_1211.json` の出所など。javap・マッピング抜粋・出所不明データは削除済み)は [THIRD_PARTY.md](THIRD_PARTY.md) にまとめています。事実の一覧であり、法的な判断ではありません。

`assets/blocks_1211.json` は PrismarineJS/minecraft-data (MIT) の `data/pc/1.21.1/blocks.json` と同一で、帰属は [NOTICE](NOTICE) に記載しています。`assets/block_states_1211.json`、`datapacks/vanilla/`（`biome_climate_tree.json` を含む）、`config_packets.bin` は 1.21.1 の server jar から `tools/vanilla_data/gen_vanilla_data.sh` で生成します([THIRD_PARTY.md](THIRD_PARTY.md) §5, §9)。
