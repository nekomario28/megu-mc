# Security Policy

Megu (megu) is a Minecraft-compatible server/client engine written in
Rust. This policy covers how to report vulnerabilities privately and what
is in scope.

日本語: 脆弱性の報告は公開 issue ではなく、本ポリシー記載の連絡先へ
私的にお願いします。詳細は末尾の「Reporting (日本語)」を参照。

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

- Preferred: use GitHub's private vulnerability reporting
  ([Security → Report a vulnerability](https://github.com/nekomario28/megu-mc/security/advisories/new))
  on this repository.
- TODO(maintainer): add a security contact email or Signal/Discord handle
  here once decided, e.g. `security@<domain>`.

Please include:

- A description of the issue and its impact.
- Steps to reproduce or a proof of concept.
- Affected version / commit SHA and platform.
- For crash-related issues, the `MEGU-E-XXXXXXXX` fingerprint and the
  redacted crash report.

We aim to acknowledge reports within **7 days** and provide a remediation
timeline once triaged. Please give us reasonable time to fix before public
disclosure.

## Scope

In scope:

- **Protocol-level denial of service** — malformed or abusive packets that
  crash or hang the server or client (`proto`, packet handlers, length
  fields, decompression limits).
- **Mod sandbox / JAR security** — the mod adapter (`mod-adapter-core`,
  `mod-offload`, `megu-mod-check --adapt-*`) and the `.class` subset
  VM / mini-JRE delegation path: escape, arbitrary code execution, unsafe
  file or network access from adapted mods.
- **Save corruption** — paths that can corrupt or destroy player saves or
  world data (`state-store`, snapshot `STV1`, world migration
  `megu-world-migrate`, region files).
- **Crash-report redaction** — leaks of home paths, IPs, tokens, or other
  secrets into crash reports or logs.
- **Authentication / session issues** — online-mode auth, Velocity
  forwarding (`MEGU_VELOCITY_FORWARDING`), op/permission checks.

Out of scope:

- Bugs requiring physical access to the host or an already-compromised
  machine.
- Issues in third-party mods themselves (report those upstream); issues in
  our *handling* of third-party mods are in scope.
- Denial of service against the operator's own infrastructure by design
  (e.g. intentionally opening the server to the public internet).

## Hardening notes for operators

See [SECURITY_NOTES.md](../SECURITY_NOTES.md) for the bind/allowlist
posture. Highlights:

- Bind to `127.0.0.1` or a private interface unless public exposure is
  intended; the published port decides exposure in containers.
- Keep Velocity forwarding off unless a proxy is actually in front.
- Review adapted mods before enabling them — `megu-mod-check` output
  is a pre-flight classification, not a security audit.
