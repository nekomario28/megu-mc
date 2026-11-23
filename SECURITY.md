# Security Policy / セキュリティポリシー

Megu (`megu-mc`) is a clean-room Rust server and native GPU client for Minecraft: Java Edition 1.21.1. This policy describes how to report security vulnerabilities privately and what is in scope.

日本語での案内は本ページ後半の「[脆弱性の報告（日本語）](#脆弱性の報告日本語)」を参照してください。

## Supported Versions

| Version | Supported |
|---|---|
| `main` / `v0.1.0-alpha.*` | Yes |

## Reporting a Vulnerability

**Do NOT open a public GitHub issue for security vulnerabilities.**

- **Preferred**: Use **[GitHub Private Vulnerability Reporting](https://github.com/nekomario28/megu-mc/security/advisories/new)** (*Security* tab → *Report a vulnerability*).
- **Email fallback**: `partners@megumin.tech` (include `[SECURITY]` in the subject line).

Please include:

- A clear description of the vulnerability and its impact.
- Minimal steps to reproduce or a proof-of-concept.
- Affected version / commit SHA and OS/platform.
- For crash-related issues, the `MEGU-E-XXXXXXXX` fingerprint and redacted crash output.

We aim to acknowledge reports within **7 days** and coordinate a fix before public disclosure.

## Scope

### In Scope
- **Protocol-level denial of service / memory safety**: Malformed packets that crash, hang, or exhaust memory on the server or client (`proto`, packet framing, decompression limits).
- **Mod inspection & sandbox boundaries**: Bypasses in static JAR security scanning (`mod-adapter-core`, `megu-mod-check`) or bounded mod execution paths.
- **World save integrity**: Bugs that corrupt or destroy world saves (`state-store`, `megu-world-migrate`).
- **Crash-report redaction**: Leaks of local file paths, IP addresses, or tokens in crash reports.
- **Authentication & session handling**: Online-mode authentication, Velocity forwarding (`MEGU_VELOCITY_FORWARDING`), and operator permission checks.

### Out of Scope
- Issues requiring existing local code execution or physical access to the host.
- Vulnerabilities inside third-party mods themselves (please report those upstream).
- Intentional operator misconfiguration (e.g., disabling authentication on a public server without a proxy). See [SECURITY_NOTES.md](SECURITY_NOTES.md) for operator hardening guidelines.

---

## 脆弱性の報告（日本語）

**セキュリティ上の脆弱性を公開 Issue に投稿しないでください。**

- **推奨窓口**: このリポジトリの **[GitHub Private Vulnerability Reporting](https://github.com/nekomario28/megu-mc/security/advisories/new)**（*Security* タブ → *Report a vulnerability*）から非公開で報告してください。
- **メール窓口**: `partners@megumin.tech`（件名に `[SECURITY]` を含めてください）。

報告時には、問題の具体的な内容・影響範囲・最小再現手順・対象バージョン（タグまたはコミット SHA）・OS 環境（クラッシュを伴う場合は `MEGU-E-XXXXXXXX` フィンガープリント）を添えてください。原則 **7 日以内** に受領確認を行い、修正完了後の公開に向けて調整します。

サーバー公開時のバインド設定やプロキシ連携などの運用上の注意点は [SECURITY_NOTES.md](SECURITY_NOTES.md) を参照してください。
