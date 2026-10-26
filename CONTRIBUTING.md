# Contributing / 貢献について

mc-spine のコードは Apache License 2.0 です ([LICENSE](LICENSE))。ライセンスの状況と公開前の確認事項は [LICENSING.md](LICENSING.md)。

- **inbound = outbound**: あなたが提出した変更 (PR・パッチ) は、Apache-2.0 (§5) で mc-spine にライセンスされるものとします。別の条件を付ける場合は、提出前にその旨を明記してください。自分が書いた、または再ライセンスする権利のあるコードだけを提出してください。
- **DCO**: コミットに `Signed-off-by: 名前 <メール>` を付ける (`git commit -s`) と、[Developer Certificate of Origin 1.1](https://developercertificate.org/) に同意したことになります。(まだ CI では強制していません。)
- **GPL/AGPL のコードを写さない**: Pumpkin (GPL-3.0) をはじめ GPL / AGPL / LGPL のソースやデータ、そこからの機械的な書き換えは取り込まないでください。アルゴリズムは vanilla の挙動・公開された論文・仕様から書き、参考にした公開情報は PR に書いてください。トークン一致の検査は `tools/provenance_scan.py`、結果は [THIRD_PARTY.md](THIRD_PARTY.md) §8。
- **Mojang 由来のデータをコミットしない**: jar・マッピング・`datapacks/vanilla/`・`config_packets.bin`・javap の出力・生成した JSON などは `.gitignore` 済みです。`git add -f` で追加しないでください。必要なデータは `tools/vanilla_data/gen_vanilla_data.sh` で手元の jar から生成します。
- **NeoForge / Fabric Loader / Mixin などの jar とソースをコミットしない**: `compat/*/dependencies.json` の URL と SHA-256 で取得します。
- **第三者のコードを入れるとき**: ライセンスを確認し、[THIRD_PARTY.md](THIRD_PARTY.md) と [NOTICE](NOTICE) に追記してください。許可リスト (`deny.toml`) に GPL 系を足さないでください。
- 秘密情報 (`.env`、鍵、トークン) をコミットしない。見つけたら [SECURITY.md](SECURITY.md) へ。

English summary: contributions are licensed under Apache-2.0 (inbound = outbound), sign off commits (DCO), do not copy GPL/AGPL code (e.g. Pumpkin), do not commit Mojang-derived data, mappings or third-party jars, and record any third-party material in THIRD_PARTY.md/NOTICE.
