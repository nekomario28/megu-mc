# Friend play and end-to-end encryption

Sharing a singleplayer world without router port forwarding, and what protects
the traffic once a guest is in: `FRL2` admission control plus the `FRL3`
end-to-end encrypted record layer.

Both layers live in the zero-dependency `proto` crate, so the same code is used
by the relay, the host and the guest.

## 1. Two layers, two jobs

| layer | where | job |
|---|---|---|
| `FRL2` | [`proto/src/friend_relay.rs`](../proto/src/friend_relay.rs) | invite codes, relay admission, `max_guests`, TTL, per-host stream multiplexing |
| `FRL2` control auth | [`proto/src/frl_auth.rs`](../proto/src/frl_auth.rs) | pure-`std` SHA-256 / HMAC-SHA256, nonce-bound join and revoke proofs |
| `FRL3` E2EE | [`proto/src/frl_e2ee.rs`](../proto/src/frl_e2ee.rs) | X25519, HKDF-SHA256, ChaCha20-Poly1305 records **inside** `StreamData` payloads |
| identity + trust store | [`client-app/src/friends_store.rs`](../client-app/src/friends_store.rs) | `megu-id:`, `megu-identity.key`, `megu-friends.toml` |
| host / guest sessions | [`client-app/src/friend_client.rs`](../client-app/src/friend_client.rs) | `FriendHostSession::register_e2ee_friends`, `FRL3` guest sessions, allowlist refresh |
| integrated world host | [`client-app/src/integrated.rs`](../client-app/src/integrated.rs) | loopback-only `megu` child for `--quickPlaySingleplayer <world>` |

Player-facing entry point that exists today:

```sh
megu-client --join-code MEGU-XXXX-XXXX --relay relay.example.org:24455
```

The relay address comes from `--relay` or `MEGU_RELAY`. The host dials
**outward**; the relay multiplexes every guest's bytes over that one host
tunnel as `stream_id`-tagged frames, so nothing on the host side listens on the
LAN or WAN.

## 2. Identity: `megu-id:`

Each install has one long-term X25519 static keypair
([`Frl3Identity`](../proto/src/frl_e2ee.rs)). The public half renders as

```
megu-id:<58 Crockford Base32 chars>
```

which is the 32-byte public key plus an IEEE **CRC32** checksum — 40 bytes
encoded, 58 characters. Parsing is tolerant on surface details (optional
`megu-id:`/`megu` prefix, case, separators) and fail-closed on a wrong length,
a non-alphabet byte or a mismatched checksum (`E2eeError::BadFriendId`). The
`Debug` rendering of the identity redacts the secret half.

`Frl3Identity::generate()` draws from the OS CSPRNG;
[`load_or_create_identity`](../client-app/src/friends_store.rs) persists it as
`<gameDir>/megu-identity.key`:

- exactly 32 bytes, staged as `megu-identity.key.part`, `fsync`ed, then renamed
  so a crash cannot leave a torn key;
- mode `0o600` forced **before** the rename, so the key is never readable with
  looser permissions at any point;
- a corrupt file (wrong length) or the degenerate all-zero secret is an error —
  never silently replaced, so a bad store cannot quietly rotate the identity.

## 3. Trust store: `megu-friends.toml`

`<gameDir>/megu-friends.toml` is the whole trust configuration:

```toml
# megu trusted friends (FRL3-FRIEND allowlist)
share_mode = "friends_only"
relay_addr = "relay.example.org:24455"

[[friends]]
name = "Alex"
friend_id = "megu-id:..."
selected_for_share = true
note = "met on the survival server"
```

- `share_mode` is one of `off`, `friends_only`, `selected_friends`
  ([`FriendShareMode`](../client-app/src/friends_store.rs)). `off` is the
  default and yields an empty allowlist.
- **Strictly one hop.** Only keys paired directly into the file are ever
  admitted — there is no "friends of friends" expansion anywhere in the code.
  A guest's display name is presentation only; the identity that counts is the
  static key the handshake proved
  (`FriendHostSession::guest_friend_id`).
- `relay_addr` remembers the relay so a paired beginner never has to pass
  `--relay` again.
- `add_friend` validates the `megu-id:` checksum, canonicalizes the stored id,
  rejects duplicates by key (not by string) and rejects adding your own key.
- Loading is fail-closed: a malformed document, an unknown `share_mode` or an
  unverifiable `friend_id` is a hard error, so trust configuration is never
  partially applied. Saves are atomic (`path.part` + rename).

## 4. The two handshake shapes

**`FRL3-PSK`** — [`Frl3GuestHandshake`](../proto/src/frl_e2ee.rs) +
`host_accept`. Ephemeral X25519 exchange bound to the 40-bit
`MEGU-XXXX-XXXX` invite code acting as a PSK: `Hello.guest_mac` proves the
guest holds the code, `Ack.host_confirm` proves the host holds the code *and*
the ephemeral secret, so a key-substituting MITM on the path fails closed.

**`FRL3-FRIEND`** — `Frl3FriendGuestHandshake` + `friend_host_accept`, a
Noise-IK / WireGuard-style mutual authentication between two long-term static
keys, with **no brute-forceable PSK at all**:

- the guest pins the host's static public key it paired with out of band;
- the host gates admission on a `FriendAllowlist` built from
  `megu-friends.toml` (optionally demanding a 128-bit `Frl3GuestToken` per
  entry via `add_with_token`; `Frl3GuestToken::NONE` means "no token");
- the session keys mix **four** Diffie-Hellman results —
  `DH(e_g,e_h) || DH(e_g,s_h) || DH(s_g,e_h) || DH(s_g,s_h)`, i.e. the
  `ee||es||se||ss` layout — so impersonating either side requires stealing a
  256-bit private key;
- an unknown static key or a mismatched token fails `UnknownPeer` *before* any
  curve work; a bad proof fails `AuthenticationFailed`; a degenerate key fails
  `ContributorySecret`. All comparisons are constant time.

Both shapes end with a **6-digit SAS**
(`Frl3Cipher::sas_code()`, `sas_code(transcript_hash) % 1_000_000`) derived
from the shared transcript, so two players can compare it out of band when
pairing.

## 5. Record layer

[`Frl3Cipher`](../proto/src/frl_e2ee.rs) is the directional record layer
carried inside `StreamData`:

```
record = u32be(ct_len) | ciphertext | tag(16)
nonce  = [dir, 0,0,0 | seq_be(8)]
AAD    = "FRL3" | dir | stream_id_be(4) | seq_be(8)
```

- HKDF-SHA256 derives per-direction keys from the handshake secret; the AEAD is
  ChaCha20-Poly1305 (RFC 8439) over a clean-room X25519 (RFC 7748).
- Every record is bound to its **direction, stream and sequence number**, so a
  replayed, reordered, cross-stream or bit-flipped record cannot verify.
- `MAX_E2EE_RECORD_BYTES` (64 KiB) is checked before any allocation, and the
  sequence counter fails closed at `u64::MAX` instead of wrapping a nonce.
- A whole `FRL2` `StreamData` payload carries one record; the client chunks
  game bytes accordingly.

## 6. What the relay sees

- **`FRL2` control frames only**: relay hello, host register/registered,
  guest join/accept/reject, stream open/close, heartbeats. The relay never sees
  the invite code after issuance and never sees an `FRL3` plaintext.
- **Game bytes are opaque ciphertext** on both links: a passive observer, an
  active path MITM or a logging relay can neither read nor forge records.
- **No IP leak between peers**: guests only ever reach the relay, and the host
  only dials outward; peers are introduced by `stream_id` inside one tunnel.
- **Weaker shape to know about:** in `FRL3-PSK` the relay itself issued the
  invite code, so it knows the PSK — a *malicious relay* stays inside the
  trust boundary there. `FRL3-FRIEND` is the shape that actually removes the
  relay from the trust boundary (static keys pinned out of band, no PSK), which
  is why pair-based hosting is the intended default.

## 7. Integrated host isolation

The friend path never turns singleplayer into a public server:

| | Integrated client host | Dedicated server |
|---|---|---|
| binary | `megu` child launched by the client | `megu` |
| bind | `INTEGRATED_BIND_IP = 127.0.0.1`, OS-assigned ephemeral port (`bind :0`) | explicit `--port` (conventional `25565`) |
| port rule | an OS-assigned port equal to `DEDICATED_SERVER_PORT` (25565) is refused | that *is* its port |
| world root | `<gameDir>/saves/<world>` after `validate_world_name` rejects separators, traversal, dot names and control characters | the operator's `--dir` / `world/` |
| lifetime | `Drop` sends `SIGTERM` so `save_world()` runs, then reaps the child | independent service |

Consequence: a player running a dedicated `megu` on `25565` can play
singleplayer or host friends on the same machine without a port collision, and
an integrated world is unreachable from the LAN even while a relay exposes it
to remote guests.

## 8. What is wired today

| surface | status |
|---|---|
| `FRL3` handshakes, record layer, `megu-id:`, identity persistence, friends store | implemented (`proto/src/frl_e2ee.rs`, `client-app/src/friends_store.rs`) |
| host/guest `FRL3` sessions, allowlist refresh, verified guest id | implemented (`client-app/src/friend_client.rs`) |
| loopback-only integrated host, dedicated-port guard, world-path validation | implemented (`client-app/src/integrated.rs`) |
| tests | `proto/tests/friend_relay_e2ee.rs`, `client-app/tests/friend_and_sync.rs` |
| CLI flags for pairing / share modes (`--join-code` is still the FRL2 path) | **not wired yet** |
| in-game friend list UI (`FriendLiveWorldEntry` is defined but unused) | **not wired yet** |

So: the cryptography and persistence are in the tree and covered by tests, but
today's player-facing command line still drives `FRL2` admission alone. Do not
promise players an E2EE toggle that the CLI does not expose.

## See also

- [operator-guide.md](operator-guide.md) — relay invocation next to the other
  sidecars, plus the bind/allowlist posture.
- [threat-model.md](threat-model.md) — the trust assumptions this page refers to.
- [../proto/src/friend_relay.rs](../proto/src/friend_relay.rs) and
  [../proto/src/frl_e2ee.rs](../proto/src/frl_e2ee.rs) — the authoritative wire
  descriptions, including the "what authentication does and does not cover"
  sections.
