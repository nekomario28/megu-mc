# Operator security and hardening

The game listener defaults to `127.0.0.1:25565`. Leave it on loopback for local play. A non-loopback bind for the game, RCON or built-in resource-pack HTTP listener requires `MEGU_ALLOW_PUBLIC_BIND=1`. This switch permits binding; it does not authenticate clients or open a firewall. Expose only the listener you intend to serve.

## Choose who authenticates players

- **Direct clients:** Enable `MEGU_ONLINE_MODE=1`. Login uses RSA key exchange and Mojang's `hasJoined` session verification; subsequent traffic uses AES-128-CFB8 encryption. The server also negotiates Zlib compression (default threshold: 256 bytes, controlled by `MEGU_COMPRESSION_THRESHOLD`). Compression is not an identity or confidentiality control.
- **Velocity proxy:** Enable Modern Forwarding on the proxy and set `MEGU_VELOCITY_FORWARDING=1` and a matching `MEGU_VELOCITY_SECRET` on the backend. Megu verifies the HMAC-SHA256 tag in constant time before trusting the forwarded name and UUID. Let the proxy authenticate clients and firewall the backend so only the proxy can reach it. Protect and rotate the shared secret.
- **Offline mode:** Names and UUIDs are not proof of identity. Duplicate logins are rejected, and `MEGU_ALLOWLIST` can restrict the names or UUIDs admitted, but neither prevents impersonation. Never expose an unauthenticated backend directly to the internet. Non-loopback offline mode without forwarding is refused unless the separate, insecure override `MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC=1` is set; do not use that override for a public service.

## Connection and input limits

| Control | What the server enforces |
| --- | --- |
| Concurrent game connections | 64 live handlers globally; `MEGU_MAX_CONN_PER_IP` defaults to 4 for non-loopback peers. An explicit per-IP limit also applies to loopback. Account for clients sharing a trusted proxy's IP. |
| Timeouts | `MEGU_LOGIN_TIMEOUT_SECS` defaults to 15 seconds for handshake, login and configuration together. In play, `MEGU_IDLE_TIMEOUT_SECS` defaults to 60 seconds without a complete packet. |
| Gameplay packet rate | A per-connection budget refills at 500 packets/s. Short bursts are tolerated; excess packets are dropped and sustained flooding causes a disconnect. |
| Framing and parsing | The play frame length is checked **before allocation** (wire maximum: 2 MiB minus one byte). Compressed packets must declare an allowed size; `inflate_exact` bounds the output and rejects length mismatches. NBT compound/list nesting is capped at 512. Ingress validation rejects malformed fields rather than relying on a panic; handler isolation is a backstop. |
| Installed content | ZIP/JAR inspection bounds archive and member sizes and expansion. Pack checks reject unsafe paths. These checks are not a sandbox for arbitrary mod code or shaders; install only content you trust. |

## Keep administration private

RCON is disabled unless both `MEGU_RCON_PORT` and a nonempty `MEGU_RCON_PASS` are configured. It binds to loopback by default. Password comparison is constant-time; failed authentication is counted per IP (`MEGU_RCON_MAX_FAILURES`, default 5), with a lockout starting at `MEGU_RCON_LOCKOUT_SECS` (default 30 seconds). **RCON is plaintext**, regardless of game encryption. Keep it on loopback or use an authenticated private tunnel; lockout cannot protect a password in transit.

## Plan for recovery

- Run Megu under a supervisor. If the authoritative tick thread unwinds, `catch_unwind` lets the process report the panic and exit with code **70** so the supervisor can restart it, rather than leaving a listener serving a stalled world.
- Each world overlay is saved via a synced temporary file and atomic rename. The previous save is kept as `.bak`; `MEGU_BACKUP_INTERVAL_SECS` (default 600) controls rotation of the older `.bak.1` and `.bak.2` generations. Separate snapshots under `backups/` are opt-in via `MEGU_BACKUP_MAX_KEEP`. Keep an off-host copy and test restoration.
- The skin lookup cache is capped at 1,024 entries. Both hits and misses expire, so arbitrary player names cannot grow the cache indefinitely.
- The shared crash reporter redacts recognized user paths, IPv4 addresses and tokens. Server tick-crash reports use this reporter in `mod-suite` builds; the fallback report includes the panic detail verbatim. Review reports and console logs before sharing, even when redaction is enabled.

See the [full threat model](docs/threat-model.md) for trust boundaries and residual risks, and the [operator guide](docs/operator-guide.md) for configuration. To report a vulnerability privately, follow [SECURITY.md](SECURITY.md); do not post it in a public issue.
