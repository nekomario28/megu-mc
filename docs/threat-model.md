# Trust boundaries / 脅威モデル

Scope: megu's Minecraft game listener, Velocity identity forwarding,
RCON, local mod/pack ingestion, and the optional Cloudflare Pages report relay.
Untrusted inputs include network packets, player names, forwarded identity
claims until verified, uploaded archives, and user-submitted diagnostics.

## Boundary inventory

| Boundary | Trusted component / secret | Validation at entry | Residual limit |
| --- | --- | --- | --- |
| Game client → server | Server configuration and locally enforced permissions. Offline-mode names are only client claims. | Packet framing and connection/login limits; `offline_public_bind_check` rejects non-loopback offline binds without online mode, forwarding, or the explicit `MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC=1` override. Online mode uses the session check and encryption path. | Offline-mode identity is unauthenticated, including on loopback. A local process or a deliberately exposed offline listener can impersonate names. Restrict network access and prefer online mode or a trusted proxy. |
| Velocity proxy → game server | `MEGU_VELOCITY_SECRET` shared with a controlled proxy. | Modern `velocity:player_info` forwarding authenticates the forwarded UUID/name with HMAC-SHA256 before the server marks the session authenticated. | The proxy and its secret are part of the trusted computing base. Keep the backend listener private so clients cannot bypass the proxy; rotate the forwarding secret if exposed. |
| RCON client → admin command executor | Operator-managed `MEGU_RCON_PASS` and the RCON bind interface. | Password compared in constant time, per-IP failed-authentication lockout, RCON disabled unless port and password are configured. Default bind is loopback. | **RCON is plaintext.** Use loopback or a VPN/private authenticated tunnel. A network observer on an exposed link can read credentials and commands; lockout does not prevent that. |
| Mod JAR / ZIP pack → loader and router | Local operator chooses which third-party mods/packs to install. | Bounded JAR scanning and archive/member inspection; `pack_router` verifies ZIP structure and member CRC, rejects traversal, unsupported/encrypted archives, excessive expansion, suspicious executable members and hazardous pack content. | Inspection is a compatibility and input-safety gate, not a guarantee that arbitrary mod bytecode or shaders are harmless. Install only trusted mods and keep file-system permissions restrictive. |
| HTTP client → Cloudflare Pages `/api/report` | Pages secrets, GitHub fine-grained PAT, configured `CRASH_INDEX` KV, and Cloudflare's client-IP header. | 4 KiB JSON cap, schema/field limits and server-side redaction; Turnstile token mandatory for **crash, bug and idea** when `TURNSTILE_SECRET_KEY` is configured; missing secret returns 503 unless `ALLOW_DEV_UNVERIFIED_REPORTS='1'`. Five requests per IP per ten minutes; KV-backed `rl:` counters and isolate-local fallback; KV dedup for all kinds. | Rate limits and dedup are best-effort across concurrent isolates because KV is eventually consistent. An absent KV binding prevents issue dedup and returns 503 for otherwise valid reports. Never deploy the development override in production. |
| Client diagnostic/report form → Pages Function → GitHub Issue | User approval to submit, restricted `GITHUB_TOKEN` with `issues:write` on the target repository. | Client-side payload preparation plus the function's independent size, schema, redaction, Turnstile and rate-limit checks. Identical crash fingerprints, bug keys and normalized idea text reuse existing issue records when KV is available. | GitHub Issues can be public; redaction is pattern-based and may miss sensitive content. Review diagnostics before submission. Duplicate issue creation remains possible under concurrent KV races. PAT revocation/rotation is an operator-runbook task. |

## Deployment responsibilities and verification boundary

- **Game exposure:** leave the game listener on loopback for local use. For
  external clients, enable online mode or verified Velocity forwarding and
  enforce network restrictions. `MEGU_ALLOW_PUBLIC_BIND=1` opts into
  network binding; `MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC=1` is a separate,
  explicitly dangerous override of the offline identity guard.
- **Control channels:** bind RCON to loopback or carry it through an access-
  controlled VPN. Keep passwords and the Velocity forwarding secret out of
  source control. Per-IP lockouts limit guessing but cannot encrypt traffic.
- **Report relay:** grant the Pages token access only to the intended GitHub
  repository with `issues:write`, provision `CRASH_INDEX` and the production
  Turnstile secret, and rotate/revoke the PAT through the operator's secret
  management runbook. `website/site-config.js` currently leaves
  `reportApiUrl` empty and the form has no Turnstile widget/token integration;
  direct production submission requires frontend integration first.
- **Evidence:** the website unit harness uses mocked Turnstile, KV and GitHub.
  These tests cover function behavior, not real edge KV consistency, proxy
  identity validation, internet deployment or mod execution isolation.

Security reporting follows [SECURITY.md](../SECURITY.md). See
[SECURITY_NOTES.md](../SECURITY_NOTES.md),
[AUDIT_WIRE.md](../AUDIT_WIRE.md), and
[operator-guide.md](operator-guide.md) for network controls and operator settings.
