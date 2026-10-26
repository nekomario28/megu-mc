# Security Policy / セキュリティポリシー

## Supported versions / サポート対象

`main` ブランチのみが対象です。リリースやタグ付きバージョンはありません (`Cargo.toml` の version `0.0.0` は復元時の値です)。
Only the `main` branch is supported. There are no tagged releases.

## Reporting a vulnerability / 脆弱性の報告

リポジトリが公開された場合は、**GitHub の Private vulnerability reporting** (リポジトリの *Security* タブ → *Report a vulnerability*) から報告してください。公開 issue には書かないでください。
(現在は非公開リポジトリです。公開する際に、Settings → Code security で *Private vulnerability reporting* を有効にしてください。)

When (and if) this repository becomes public, please report vulnerabilities privately through GitHub's *Private vulnerability reporting* (Security tab → *Report a vulnerability*), not in a public issue. The repository is currently private; the owner should enable that setting before making it public.

## Scope and known limitations / 既知の制約

これは実験的なサーバーです。認証 (オンラインモード)・通信の暗号化・接続レート制限などは未実装で、既定では `127.0.0.1` にのみ bind します。**インターネットへ公開しないでください。** 既知の問題、過去の事故と対策の記録は [SECURITY_NOTES.md](SECURITY_NOTES.md) と [AUDIT_WIRE.md](AUDIT_WIRE.md) を参照してください。

This is an experimental server with no authentication, encryption, or per-IP rate limiting; it binds to loopback by default and must not be exposed to the internet. See [SECURITY_NOTES.md](SECURITY_NOTES.md) for known issues and history.

Third-party data provenance questions are tracked in [THIRD_PARTY.md](THIRD_PARTY.md).
