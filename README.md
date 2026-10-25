# mc-spine

Minecraft Java 1.21.1（protocol 767）向けの実験的なRustサーバーです。小規模・低負荷の世界で、観測できる挙動を保ちながら計算量を減らし、必要な既存Modとの接続を試しています。**Minecraft全体との互換性や、vanillaより軽いことが確認済みの製品ではありません。**

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

コードはMIT（[LICENSE](LICENSE)）。`datapacks/vanilla/`、`assets/`、`resourcepack.zip`、`config_packets.bin` はMojang由来のデータ・資産であり、MITライセンスの対象外です。詳細は[復元と検証の記録](RESTORATION.md)と[第三者データの注意](SECURITY_NOTES.md)を参照してください。GPL/AGPLの実装コードは取り込んでいません。

各ファイルの出所の一覧と、オーナー確認待ちの項目(出所が文書化されていないデータ、`evidence/gameplay/` の javap 逆アセンブル・Mojang 公式マッピング抜粋、Rust 依存のライセンス)は [THIRD_PARTY.md](THIRD_PARTY.md) にまとめています。事実の一覧であり、法的な判断ではありません。

`assets/blocks_1211.json` は PrismarineJS/minecraft-data (MIT) の `data/pc/1.21.1/blocks.json` と同一で、帰属は [NOTICE](NOTICE) に記載しています。`assets/block_states_1211.json` と `datapacks/vanilla/biome_climate_tree.json` は 1.21.1 の server jar から `tools/vanilla_data/gen_vanilla_data.sh` で再生成できます([THIRD_PARTY.md](THIRD_PARTY.md) §5)。
