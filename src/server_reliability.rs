// ---------- real-world server must-haves ----------
//
// Core public-server security and recovery mechanisms. Online-mode RSA uses
// the pinned rsa/rand dependencies; the remaining primitives live here.
//
// * online-mode login cryptography: FIPS-197 AES-128 in the CFB8 stream mode the
//   Java protocol uses, the signed SHA-1 "server hash" Mojang's session server
//   expects, and the protocol-767 `0x01` Encryption Request/Response codecs.
// * Velocity modern forwarding (`velocity:player_info`): HMAC-SHA256 over the
//   forwarded login payload, with constant-time tag comparison.
// * Per-connection crash isolation: `run_isolated_connection` turns a panicking
//   handler into a logged, counted disconnect instead of a dead server.
// * `AsyncSaveWorker`: one background writer fed by a single coalescing slot, so a
//   slow disk can never grow an unbounded queue behind the 20 TPS tick loop, plus
//   `rotate_world_backups` for bounded `backups/` retention. Sequence numbers, not
//   wall-clock reads, name the files.
// * `DeltaWalJournal`: a framed, CRC-checked, `sync_data`ed append log of world
//   deltas so a crash loses near-zero seconds between the five-minute saves.
// * `TickFaultQuarantine`: a panicking block entity, entity or chunk tick is
//   caught and only that broken target frozen, keeping the world online.
//
// Nothing here reads a wall clock: save/backup naming comes from a monotonic
// sequence drawn from the directory itself, which survives restarts.

// ================= AES-128 (FIPS-197) in CFB8 mode =================
//
// Minecraft encrypts login onward with AES/CFB8/NoPadding: a 128-bit block cipher
// whose keystream byte 0 is XORed into one data byte at a time, with the produced
// ciphertext byte shifted into the 16-byte feedback register. Only the forward
// cipher is ever needed - decryption reuses it.

const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];
const AES_RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// GF(2^8) doubling modulo the AES polynomial.
fn aes_xtime(x: u8) -> u8 {
    if x & 0x80 != 0 {
        (x << 1) ^ 0x1b
    } else {
        x << 1
    }
}

/// FIPS-197 §5.2 key expansion: 11 round keys of 16 bytes for AES-128.
fn aes128_expand_key(key: &[u8; 16]) -> [u8; 176] {
    let mut w = [0u8; 176];
    w[..16].copy_from_slice(key);
    let mut i = 16;
    let mut round = 0usize;
    while i < 176 {
        let mut t = [w[i - 4], w[i - 3], w[i - 2], w[i - 1]];
        if i % 16 == 0 {
            t = [
                AES_SBOX[t[1] as usize] ^ AES_RCON[round],
                AES_SBOX[t[2] as usize],
                AES_SBOX[t[3] as usize],
                AES_SBOX[t[0] as usize],
            ];
            round += 1;
        }
        for j in 0..4 {
            w[i + j] = w[i - 16 + j] ^ t[j];
        }
        i += 4;
    }
    w
}

/// One FIPS-197 forward cipher block. State is column-major: index `4 * col + row`.
fn aes128_encrypt_block(round_keys: &[u8; 176], input: &[u8; 16]) -> [u8; 16] {
    let mut s = [0u8; 16];
    for i in 0..16 {
        s[i] = input[i] ^ round_keys[i];
    }
    for round in 1..10 {
        // SubBytes fused with ShiftRows: row r at output column c reads input
        // column (c + r) mod 4.
        let mut t = [0u8; 16];
        for c in 0..4 {
            for r in 0..4 {
                t[4 * c + r] = AES_SBOX[s[4 * ((c + r) % 4) + r] as usize];
            }
        }
        // MixColumns: [2 3 1 1; 1 2 3 1; 1 1 2 3; 3 1 1 2] over GF(2^8).
        for c in 0..4 {
            let (a0, a1, a2, a3) = (t[4 * c], t[4 * c + 1], t[4 * c + 2], t[4 * c + 3]);
            s[4 * c] = aes_xtime(a0) ^ (aes_xtime(a1) ^ a1) ^ a2 ^ a3;
            s[4 * c + 1] = a0 ^ aes_xtime(a1) ^ (aes_xtime(a2) ^ a2) ^ a3;
            s[4 * c + 2] = a0 ^ a1 ^ aes_xtime(a2) ^ (aes_xtime(a3) ^ a3);
            s[4 * c + 3] = (aes_xtime(a0) ^ a0) ^ a1 ^ a2 ^ aes_xtime(a3);
        }
        for i in 0..16 {
            s[i] ^= round_keys[16 * round + i];
        }
    }
    let mut out = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[4 * c + r] = AES_SBOX[s[4 * ((c + r) % 4) + r] as usize];
        }
    }
    for i in 0..16 {
        out[i] ^= round_keys[160 + i];
    }
    out
}

/// AES-128/CFB8 stream state. Minecraft uses the shared secret as both key and IV.
/// One direction per instance: a connection needs one encryptor and one decryptor.
pub(crate) struct Aes128Cfb8 {
    round_keys: [u8; 176],
    feedback: [u8; 16],
}

impl Aes128Cfb8 {
    pub(crate) fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        Aes128Cfb8 {
            round_keys: aes128_expand_key(key),
            feedback: *iv,
        }
    }
    /// Encrypt in place; each produced byte feeds back for the next keystream byte.
    pub(crate) fn encrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            let keystream = aes128_encrypt_block(&self.round_keys, &self.feedback);
            let ct = *byte ^ keystream[0];
            self.feedback.copy_within(1.., 0);
            self.feedback[15] = ct;
            *byte = ct;
        }
    }
    /// Decrypt in place; the *input* byte feeds back, not the recovered plaintext.
    pub(crate) fn decrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            let keystream = aes128_encrypt_block(&self.round_keys, &self.feedback);
            let ct = *byte;
            *byte = ct ^ keystream[0];
            self.feedback.copy_within(1.., 0);
            self.feedback[15] = ct;
        }
    }
}

// ================= Minecraft signed SHA-1 server hash =================
//
// `minecraft:hasJoined` is looked up by a hash of serverId || sharedSecret ||
// publicKey printed the way Java's `new BigInteger(digest).toString(16)` prints
// it: big-endian two's complement, a leading `-` when the top bit is set, and the
// magnitude is the negated value with leading zeros stripped.

/// Render a SHA-1 digest as Java `BigInteger.toString(16)` would.
fn mc_signed_hex_digest(digest: &[u8; 20]) -> String {
    let negative = digest[0] & 0x80 != 0;
    let mut bytes = *digest;
    if negative {
        let mut carry = 1u16;
        for b in bytes.iter_mut().rev() {
            let v = u16::from(!*b) + carry;
            *b = v as u8;
            carry = v >> 8;
        }
    }
    let digits = hex(&bytes);
    let trimmed = digits.trim_start_matches('0');
    let magnitude = if trimmed.is_empty() { "0" } else { trimmed };
    if negative {
        format!("-{magnitude}")
    } else {
        magnitude.to_string()
    }
}

/// The `serverId` argument Mojang's `hasJoined` endpoint expects for this login.
/// Vanilla always passes an empty `server_id` string in 1.21.x.
pub(crate) fn mc_server_hash(server_id: &str, shared_secret: &[u8], public_key_der: &[u8]) -> String {
    let mut data = Vec::with_capacity(server_id.len() + shared_secret.len() + public_key_der.len());
    data.extend_from_slice(server_id.as_bytes());
    data.extend_from_slice(shared_secret);
    data.extend_from_slice(public_key_der);
    mc_signed_hex_digest(&sha1(&data))
}

// ================= HMAC-SHA256 (RFC 2104) =================

/// RFC 2104 HMAC over the existing FIPS 180-4 SHA-256 primitive.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&generation_math::sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + message.len());
    inner.extend(block.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(message);
    let inner_hash = generation_math::sha256(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend(block.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    generation_math::sha256(&outer)
}

/// Constant-time byte equality for MAC tags: mismatch length fails fast, matching
/// length never branches on content.
fn ct_tag_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ================= login encryption packet codecs (protocol 767) =================
//
// S2C Login `0x01` Encryption Request: serverId String, public key byte array,
// verify-token byte array, shouldAuthenticate bool. C2S `0x01` Encryption
// Response: two byte arrays (encrypted shared secret, encrypted verify token).

pub(crate) const LOGIN_ENCRYPTION_REQUEST_ID: i64 = 0x01;
pub(crate) const LOGIN_ENCRYPTION_RESPONSE_ID: i64 = 0x01;

/// Protocol byte array: VarInt length then that many bytes.
fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len() as i64);
    out.extend_from_slice(bytes);
}

/// Bounded read of a protocol byte array; `max` stops a forged length.
fn read_byte_array(b: &mut &[u8], max: usize) -> Option<Vec<u8>> {
    let len = read_protocol_i32(b)?;
    if len < 0 || len as usize > max || b.len() < len as usize {
        return None;
    }
    let (head, rest) = b.split_at(len as usize);
    *b = rest;
    Some(head.to_vec())
}

/// Bounded read of a protocol string (VarInt length + UTF-8).
fn read_string_bounded(b: &mut &[u8], max: usize) -> Option<String> {
    let bytes = read_byte_array(b, max)?;
    String::from_utf8(bytes).ok()
}

/// The body of S2C Login `0x01` (packet id is written by `send_packet`).
pub(crate) fn encode_encryption_request(
    server_id: &str,
    public_key_der: &[u8],
    verify_token: &[u8],
    should_authenticate: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    write_str(&mut out, server_id);
    write_byte_array(&mut out, public_key_der);
    write_byte_array(&mut out, verify_token);
    out.push(u8::from(should_authenticate));
    out
}

/// Peer view of an Encryption Request body, e.g. for a proxy-facing decoder.
#[cfg(test)]
pub(crate) fn decode_encryption_request(
    body: &[u8],
) -> Option<(String, Vec<u8>, Vec<u8>, bool)> {
    let mut b = body;
    let server_id = read_string_bounded(&mut b, 64)?;
    let public_key_der = read_byte_array(&mut b, 8192)?;
    let verify_token = read_byte_array(&mut b, 1024)?;
    let should_authenticate = match b.first() {
        Some(&0) => false,
        Some(&1) => true,
        _ => return None,
    };
    Some((server_id, public_key_der, verify_token, should_authenticate))
}

/// The body of C2S Login `0x01` Encryption Response.
#[cfg(test)]
pub(crate) fn encode_encryption_response(
    encrypted_shared_secret: &[u8],
    encrypted_verify_token: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    write_byte_array(&mut out, encrypted_shared_secret);
    write_byte_array(&mut out, encrypted_verify_token);
    out
}

/// Returns `(encrypted_shared_secret, encrypted_verify_token)`. RSA-1024 output
/// caps each at 128 bytes for the vanilla server; generous bounds only.
pub(crate) fn decode_encryption_response(body: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut b = body;
    let secret = read_byte_array(&mut b, 4096)?;
    let token = read_byte_array(&mut b, 4096)?;
    Some((secret, token))
}

// ================= Velocity modern forwarding =================
//
// `velocity:player_info` Login Plugin Response payload:
//   [32-byte HMAC-SHA256 over the rest, keyed by the forwarding secret]
//   VarInt forwarding version (1..=4 supported by Velocity)
//   String forwarded client address
//   16-byte UUID
//   String username
//   VarInt property count, then name/value[/signature] triples
//
// Verification happens before any field is trusted: a wrong secret or one flipped
// byte rejects the whole payload, so a direct connection can never forge a
// premium identity.

/// A skin/profile property forwarded by the proxy (`textures` in practice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForwardedProperty {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) signature: Option<String>,
}

/// The verified contents of a Velocity modern-forwarding payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VelocityForwardedPlayer {
    pub(crate) version: i32,
    pub(crate) forwarded_ip: String,
    pub(crate) uuid: [u8; 16],
    pub(crate) username: String,
    pub(crate) properties: Vec<ForwardedProperty>,
}

pub(crate) const VELOCITY_FORWARDING_CHANNEL: &str = "velocity:player_info";
pub(crate) const VELOCITY_FORWARDING_VERSION_MAX: i32 = 4;

/// Verify and decode a `velocity:player_info` payload. Errors are deliberately
/// undifferentiated enough to log without giving a forger an oracle.
pub(crate) fn verify_velocity_forwarding(
    secret: &[u8],
    payload: &[u8],
) -> Result<VelocityForwardedPlayer, String> {
    if secret.is_empty() {
        return Err("velocity forwarding: empty secret".into());
    }
    if payload.len() <= 32 {
        return Err("velocity forwarding: payload shorter than its signature".into());
    }
    let (tag, body) = payload.split_at(32);
    if !ct_tag_eq(tag, &hmac_sha256(secret, body)) {
        return Err("velocity forwarding: signature mismatch".into());
    }
    let mut b = body;
    let version = read_protocol_i32(&mut b)
        .ok_or_else(|| "velocity forwarding: truncated version".to_string())?;
    if !(1..=VELOCITY_FORWARDING_VERSION_MAX).contains(&version) {
        return Err(format!("velocity forwarding: unsupported version {version}"));
    }
    let forwarded_ip = read_string_bounded(&mut b, 256)
        .ok_or_else(|| "velocity forwarding: bad address".to_string())?;
    if b.len() < 16 {
        return Err("velocity forwarding: truncated uuid".into());
    }
    let uuid: [u8; 16] = b[..16]
        .try_into()
        .map_err(|_| "velocity forwarding: truncated uuid".to_string())?;
    b = &b[16..];
    let username = read_string_bounded(&mut b, 64)
        .ok_or_else(|| "velocity forwarding: bad username".to_string())?;
    let count = read_protocol_i32(&mut b)
        .ok_or_else(|| "velocity forwarding: bad property count".to_string())?;
    if !(0..=1024).contains(&count) {
        return Err("velocity forwarding: property count out of range".into());
    }
    let mut properties = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = read_string_bounded(&mut b, 32767)
            .ok_or_else(|| "velocity forwarding: bad property name".to_string())?;
        let value = read_string_bounded(&mut b, 32767)
            .ok_or_else(|| "velocity forwarding: bad property value".to_string())?;
        let signature = match b.first() {
            Some(&0) => {
                b = &b[1..];
                None
            }
            Some(&1) => {
                b = &b[1..];
                Some(
                    read_string_bounded(&mut b, 32767)
                        .ok_or_else(|| "velocity forwarding: bad signature".to_string())?,
                )
            }
            _ => return Err("velocity forwarding: bad signature flag".into()),
        };
        properties.push(ForwardedProperty {
            name,
            value,
            signature,
        });
    }
    Ok(VelocityForwardedPlayer {
        version,
        forwarded_ip,
        uuid,
        username,
        properties,
    })
}

// ================= Mojang session-server verification =================
//
// After the client answers the Encryption Response the server asks
// `sessionserver.mojang.com/session/minecraft/hasJoined` whether that account
// really joined this server hash. HTTP stays behind a transport closure so the
// unit tests never touch the network.

/// A verified Mojang session profile as `hasJoined` returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MojangJoinedProfile {
    pub(crate) uuid: [u8; 16],
    pub(crate) name: String,
    pub(crate) properties: Vec<ForwardedProperty>,
}

/// Undashed 32-hex `id` field -> raw 16-byte UUID.
fn parse_mojang_uuid(id: &str) -> Option<[u8; 16]> {
    let nibbles: Vec<u8> = id
        .bytes()
        .map(|c| match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => 0xff,
        })
        .collect();
    if nibbles.len() != 32 || nibbles.iter().any(|&n| n > 0x0f) {
        return None;
    }
    let mut uuid = [0u8; 16];
    for i in 0..16 {
        uuid[i] = (nibbles[2 * i] << 4) | nibbles[2 * i + 1];
    }
    Some(uuid)
}

/// Percent-encode for query parameters: unreserved characters pass through.
fn url_query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The real GET against Mojang; `Ok(None)` is the 204 "no such session" answer.
fn mojang_http_get(url: &str) -> Result<Option<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .into();
    match agent.get(url).call() {
        Ok(mut response) => {
            if response.status().as_u16() == 204 {
                return Ok(None);
            }
            response
                .body_mut()
                .with_config()
                .limit(1 << 20)
                .read_to_string()
                .map(Some)
                .map_err(|e| format!("sessionserver read failed: {e}"))
        }
        Err(ureq::Error::StatusCode(204)) => Ok(None),
        Err(e) => Err(format!("sessionserver request failed: {e}")),
    }
}

/// Parse the `hasJoined` JSON body: `{id, name, properties:[{name,value,signature?}]}`.
fn parse_joined_profile(body: &str) -> Result<MojangJoinedProfile, String> {
    let j = jparse(body.as_bytes()).ok_or_else(|| "sessionserver: invalid JSON".to_string())?;
    let id = json::jstr(&j, "id").ok_or_else(|| "sessionserver: missing id".to_string())?;
    let uuid = parse_mojang_uuid(&id).ok_or_else(|| "sessionserver: malformed id".to_string())?;
    let name =
        json::jstr(&j, "name").ok_or_else(|| "sessionserver: missing name".to_string())?;
    let mut properties = Vec::new();
    if let Some(J::Arr(items)) = json::js(&j, "properties") {
        for item in items {
            let pname = json::jstr(item, "name")
                .ok_or_else(|| "sessionserver: property without name".to_string())?;
            let value = json::jstr(item, "value")
                .ok_or_else(|| "sessionserver: property without value".to_string())?;
            properties.push(ForwardedProperty {
                name: pname,
                value,
                signature: json::jstr(item, "signature"),
            });
        }
    }
    Ok(MojangJoinedProfile {
        uuid,
        name,
        properties,
    })
}

/// Transport-injected core so tests drive it offline: `transport(url)` returns
/// the response body, or `None` for a 204.
fn verify_mojang_has_joined_transport(
    username: &str,
    server_hash: &str,
    prevent_proxy_ip: Option<&str>,
    transport: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<MojangJoinedProfile, String> {
    if username.is_empty() || username.len() > 16 {
        return Err("hasJoined: username out of range".into());
    }
    let mut url = format!(
        "https://sessionserver.mojang.com/session/minecraft/hasJoined?username={}&serverId={}",
        url_query_escape(username),
        url_query_escape(server_hash)
    );
    if let Some(ip) = prevent_proxy_ip {
        url.push_str(&format!("&ip={}", url_query_escape(ip)));
    }
    match transport(&url)? {
        None => Err("sessionserver: no matching session (204)".into()),
        Some(body) => parse_joined_profile(&body),
    }
}

/// Online-mode check: has `username` joined this `server_hash`? When
/// `prevent_proxy_ip` is `Some`, Mojang also verifies the client address, which
// blocks players routing through another host to dodge IP bans.
pub(crate) fn verify_mojang_has_joined(
    username: &str,
    server_hash: &str,
    prevent_proxy_ip: Option<&str>,
) -> Result<MojangJoinedProfile, String> {
    verify_mojang_has_joined_transport(username, server_hash, prevent_proxy_ip, &mojang_http_get)
}

// ================= per-connection crash isolation =================
//
// A player connection runs arbitrary packet handling; a panic there must cost
// that player their connection, not the whole process and everyone else on it.

/// Connections ended by an isolated panic since startup. Operators watch this:
/// zero forever is healthy, a growing number names a real bug.
pub(crate) static ISOLATED_CONNECTION_DISCONNECTS: AtomicU64 = AtomicU64::new(0);

/// Pull a readable message out of a panic payload for the operator log.
fn panic_payload_message(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

/// Run one connection's handler so a panic inside it becomes a clean disconnect:
/// the counter is bumped, one operator-friendly line is logged, `cleanup` runs
/// (drop the socket, release the slot), and `None` comes back. A normal return is
/// `Some` of whatever `run` produced. The panic never propagates, so the listener
/// and every other connection survive.
pub(crate) fn run_isolated_connection<F, C, T>(label: &str, run: F, cleanup: C) -> Option<T>
where
    F: FnOnce() -> T,
    C: FnOnce(),
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(value) => Some(value),
        Err(payload) => {
            ISOLATED_CONNECTION_DISCONNECTS.fetch_add(1, Ordering::SeqCst);
            eprintln!(
                "connection '{label}' disconnected: handler panicked ({})",
                panic_payload_message(&payload)
            );
            cleanup();
            None
        }
    }
}

// ================= connection transport: plain or AES-128-CFB8 =================
//
// One connection's bytes either go through the socket untouched (offline /
// pre-encryption login) or through an AES-128-CFB8 pair keyed by the shared
// secret negotiated in the Encryption Response. `Plain` costs one match arm
// per call; `Encrypted` adds an in-place cipher pass per read/write.

// Keep the encrypted stream inline to retain the existing connection layout.
#[allow(clippy::large_enum_variant)]
#[cfg(test)]
pub(crate) enum ConnStream {
    Plain(TcpStream),
    Encrypted {
        stream: TcpStream,
        rx: Aes128Cfb8,
        tx: Aes128Cfb8,
    },
}

#[cfg(test)]
impl ConnStream {
    /// The socket underneath either lane, for timeouts, shutdown and peer
    /// lookups the transport enum itself does not model.
    #[cfg(test)]
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            ConnStream::Plain(s) => s,
            ConnStream::Encrypted { stream, .. } => stream,
        }
    }

    /// Switch a still-plain transport to encrypted with the negotiated shared
    /// secret (key == IV, per the Java protocol). Fails on an already-encrypted
    /// stream rather than silently restarting the keystream.
    #[cfg(all(test, megu_vanilla_data))]
    pub(crate) fn enable_encryption(&mut self, secret: &[u8; 16]) -> std::io::Result<()> {
        let ConnStream::Plain(stream) = self else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "connection already runs encrypted",
            ));
        };
        let stream = stream.try_clone()?;
        *self = ConnStream::Encrypted {
            stream,
            rx: Aes128Cfb8::new(secret, secret),
            tx: Aes128Cfb8::new(secret, secret),
        };
        Ok(())
    }
}

#[cfg(test)]
impl From<TcpStream> for ConnStream {
    fn from(stream: TcpStream) -> Self {
        ConnStream::Plain(stream)
    }
}

#[cfg(test)]
impl Read for ConnStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConnStream::Plain(s) => s.read(out),
            ConnStream::Encrypted { stream, rx, .. } => {
                let n = stream.read(out)?;
                rx.decrypt(&mut out[..n]);
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
impl Write for ConnStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConnStream::Plain(s) => s.write(buf),
            ConnStream::Encrypted { stream, tx, .. } => {
                // The cipher consumes exactly what leaves the socket; encrypt
                // the slice and report it written only once it all went out.
                let mut encrypted = buf.to_vec();
                tx.encrypt(&mut encrypted);
                stream.write_all(&encrypted)?;
                Ok(buf.len())
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConnStream::Plain(s) => s.flush(),
            ConnStream::Encrypted { stream, .. } => stream.flush(),
        }
    }
}

// ================= online-mode provider slot =================
//
// The login handshake keeps RSA key handling and Mojang's session check behind
// a startup-installed provider so tests can substitute a deterministic one.
// Missing providers fail every online-mode login closed.

/// The keypair and session-verification half of online-mode login.
pub(crate) trait OnlineAuth: Send + Sync {
    /// DER-encoded public key sent in the Encryption Request.
    fn public_key_der(&self) -> Vec<u8>;
    /// Unwrap one client-encrypted field of the Encryption Response.
    fn decrypt_login_field(&self, ciphertext: &[u8]) -> Option<Vec<u8>>;
    /// Fresh per-handshake bytes the client must echo back encrypted.
    fn verify_token(&self) -> Vec<u8>;
    /// `sessionserver` hasJoined for `username`/`server_hash`; `client_ip` is
    /// the optional prevent-proxy IP argument.
    fn has_joined(
        &self,
        username: &str,
        server_hash: &str,
        client_ip: Option<&str>,
    ) -> Result<MojangJoinedProfile, String>;
}

/// One process-wide vanilla RSA-1024 keypair, shared by online-mode logins.
/// Creating it at startup avoids expensive key generation for each handshake.
pub(crate) struct RsaOnlineAuth {
    private_key: rsa::RsaPrivateKey,
    public_der: Vec<u8>,
}

impl RsaOnlineAuth {
    pub(crate) fn new() -> Result<Self, String> {
        use rsa::pkcs8::EncodePublicKey;

        let private_key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024)
            .map_err(|e| format!("online-mode RSA key generation failed: {e}"))?;
        let public_der = rsa::RsaPublicKey::from(&private_key)
            .to_public_key_der()
            .map_err(|e| format!("online-mode RSA public key encoding failed: {e}"))?
            .as_ref()
            .to_vec();
        Ok(Self {
            private_key,
            public_der,
        })
    }
}

impl OnlineAuth for RsaOnlineAuth {
    fn public_key_der(&self) -> Vec<u8> {
        self.public_der.clone()
    }

    fn decrypt_login_field(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        self.private_key.decrypt(rsa::Pkcs1v15Encrypt, ciphertext).ok()
    }

    fn verify_token(&self) -> Vec<u8> {
        use rand::RngCore;

        // Each client receives its own unpredictable challenge to prevent replay.
        let mut token = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut token);
        token.to_vec()
    }

    fn has_joined(
        &self,
        username: &str,
        server_hash: &str,
        client_ip: Option<&str>,
    ) -> Result<MojangJoinedProfile, String> {
        verify_mojang_has_joined(username, server_hash, client_ip)
    }
}

#[cfg(test)]
mod rsa_online_auth_tests {
    use super::{OnlineAuth, RsaOnlineAuth};
    use rsa::pkcs8::DecodePublicKey;
    use rsa::traits::PublicKeyParts;

    #[test]
    fn rsa_online_auth_roundtrips_vanilla_encryption_fields() {
        let auth = RsaOnlineAuth::new().expect("create server keypair");
        let public_key = rsa::RsaPublicKey::from_public_key_der(&auth.public_key_der())
            .expect("decode vanilla public key DER");
        assert_eq!(public_key.size(), 128);

        let token = auth.verify_token();
        assert_eq!(token.len(), 4);
        for payload in [token.as_slice(), b"0123456789abcdef".as_slice()] {
            let ciphertext = public_key
                .encrypt(&mut rand::rngs::OsRng, rsa::Pkcs1v15Encrypt, payload)
                .expect("encrypt client payload");
            assert_eq!(auth.decrypt_login_field(&ciphertext).as_deref(), Some(payload));
        }
        assert!(auth.decrypt_login_field(&[]).is_none());
    }
}

fn online_auth_slot() -> &'static Mutex<Option<Arc<dyn OnlineAuth>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<dyn OnlineAuth>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or clear) the online-mode provider. Startup wiring and tests use
/// this; the login path reads through `online_auth_provider`.
pub(crate) fn set_online_auth_provider(provider: Option<Arc<dyn OnlineAuth>>) {
    *online_auth_slot().lock().unwrap_or_else(|e| e.into_inner()) = provider;
}

/// The installed provider, or `None` when online mode must fail closed.
pub(crate) fn online_auth_provider() -> Option<Arc<dyn OnlineAuth>> {
    online_auth_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ================= async background autosave =================
//
// The tick loop produces an immutable serialized snapshot and hands it to one
// writer thread through a single slot: if the disk is still busy when the next
// snapshot is ready the pending one is *replaced*, never queued, so memory stays
// bounded no matter how slow the disk gets. Every write is temp-file + sync +
// rename so a crash can leave the old or the new file, never a torn one.

/// One unit of save work: the target path and the already-serialized bytes.
pub(crate) struct SaveJob {
    pub(crate) path: std::path::PathBuf,
    pub(crate) bytes: std::sync::Arc<[u8]>,
    pub(crate) seq: u64,
}

struct SaveWorkerState {
    /// At most one job waits: a newer snapshot supersedes the pending one.
    pending: Option<SaveJob>,
    /// Set on drop; the worker drains the pending job before exiting.
    closing: bool,
    /// Test hook: hold the worker off the slot so coalescing is deterministic.
    paused: bool,
    /// Sequence number of the most recently finished write.
    last_done: u64,
    /// Total writes finished, counting coalesced-away jobs as dropped.
    writes: u64,
    /// First unreported write failure, returned by the next `flush`.
    last_error: Option<String>,
}

/// Temp + rename write with an fsync before the rename, same discipline as
/// `ops_json_write_atomic`.
fn write_bytes_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|_| std::fs::rename(&temp, path)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Background snapshot writer. `submit` never blocks the tick loop; `flush`
/// waits for everything submitted so far; `Drop` finishes the pending job and
/// joins the thread, so shutdown paths keep the final save.
pub(crate) struct AsyncSaveWorker {
    state: std::sync::Arc<(Mutex<SaveWorkerState>, std::sync::Condvar)>,
    next_seq: AtomicU64,
    thread: Option<thread::JoinHandle<()>>,
}

impl AsyncSaveWorker {
    pub(crate) fn spawn() -> Self {
        let state = std::sync::Arc::new((
            Mutex::new(SaveWorkerState {
                pending: None,
                closing: false,
                paused: false,
                last_done: 0,
                writes: 0,
                last_error: None,
            }),
            std::sync::Condvar::new(),
        ));
        let worker_state = std::sync::Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("megu-save".into())
            .spawn(move || Self::run(&worker_state))
            .expect("spawn save worker");
        AsyncSaveWorker {
            state,
            next_seq: AtomicU64::new(1),
            thread: Some(thread),
        }
    }

    fn run(state: &std::sync::Arc<(Mutex<SaveWorkerState>, std::sync::Condvar)>) {
        let (lock, cv) = &**state;
        loop {
            let job = {
                let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if !g.paused || g.closing {
                        if let Some(job) = g.pending.take() {
                            break job;
                        }
                    }
                    if g.closing {
                        return;
                    }
                    g = cv.wait(g).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let result = write_bytes_atomic(&job.path, &job.bytes);
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            g.writes += 1;
            g.last_done = job.seq;
            if let Err(error) = result {
                if g.last_error.is_none() {
                    g.last_error = Some(format!("{}: {error}", job.path.display()));
                }
                eprintln!("async save to {} failed: {error}", job.path.display());
            }
            cv.notify_all();
        }
    }

    /// Queue a snapshot, replacing any still-pending one. Returns the job's
    /// sequence number for `flush`.
    #[cfg(test)]
    pub(crate) fn submit(&self, path: impl Into<std::path::PathBuf>, bytes: std::sync::Arc<[u8]>) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).pending = Some(SaveJob {
            path: path.into(),
            bytes,
            seq,
        });
        cv.notify_one();
        seq
    }

    /// Sequence number a `flush` must reach to cover everything submitted so far.
    pub(crate) fn submitted_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }

    /// Wait until every job submitted before this call is written or superseded.
    /// Returns the worker's first unreported write error, if any.
    pub(crate) fn flush(&self) -> std::io::Result<()> {
        let target = self.submitted_seq();
        let (lock, cv) = &*self.state;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        while g.last_done < target {
            g = cv.wait(g).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        match g.last_error.take() {
            Some(message) => Err(std::io::Error::other(message)),
            None => Ok(()),
        }
    }

    /// Completed writes, for tests and operator telemetry.
    #[cfg(test)]
    pub(crate) fn writes_completed(&self) -> u64 {
        self.state.0.lock().unwrap_or_else(|e| e.into_inner()).writes
    }

    /// Pause/resume draining. Used by tests to force coalescing deterministically;
    /// shutdown still drains a pending job regardless.
    #[cfg(test)]
    pub(crate) fn set_paused(&self, paused: bool) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).paused = paused;
        cv.notify_all();
    }
}

impl std::ops::Drop for AsyncSaveWorker {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closing = true;
        cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Write `bytes` as the newest `{prefix}-{seq:08}.bak` under `dir`, then prune
/// all but the `max_keep` newest backups of that prefix. `seq` is one past the
/// highest sequence already in the directory, so names stay monotonic across
/// restarts without touching a wall clock. Returns the path written.
#[cfg(test)]
pub(crate) fn rotate_world_backups(
    dir: &std::path::Path,
    name_prefix: &str,
    bytes: &[u8],
    max_keep: usize,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let marker = format!("{name_prefix}-");
    let mut existing: Vec<(u64, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&marker) else {
            continue;
        };
        let Some(seq_text) = rest.strip_suffix(".bak") else {
            continue;
        };
        if let Ok(seq) = seq_text.parse::<u64>() {
            existing.push((seq, entry.path()));
        }
    }
    let seq = existing.iter().map(|(seq, _)| *seq).max().unwrap_or(0) + 1;
    let path = dir.join(format!("{marker}{seq:08}.bak"));
    write_bytes_atomic(&path, bytes)?;
    existing.push((seq, path.clone()));
    existing.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, old) in existing.into_iter().skip(max_keep) {
        let _ = std::fs::remove_file(old);
    }
    Ok(path)
}

// ================= crash-minimizing delta WAL (DWAL) =================
//
// A full world save runs on a slow timer; without a journal a crash loses the
// whole window. This append-only log records the small deltas (block sets,
// player snapshots, container slots) as they happen, each frame `sync_data()`ed
// to disk, so crash loss is near-zero seconds instead of minutes. Recovery
// replays only what the last committed checkpoint did not already cover;
// `compact_after_checkpoint` shrinks the log once the full save lands.
//
// Frame layout, all big-endian:
//   magic `DWAL` (4) | u64 sequence | u32 payload_len | payload | u32 crc32
// where the CRC is reflected IEEE 802.3 (poly 0xEDB88320) over
// seq||payload_len||payload. The only forgivable damage is a torn tail - an
// append cut short by power loss - which is reported and skipped. Anything
// corrupt mid-stream fails closed, and a forged payload_len can never reserve
// memory: the cap is checked before any allocation.

/// Largest single WAL payload. Checked before the reader allocates, so a
/// corrupt length field cannot reserve gigabytes.
pub(crate) const MAX_WAL_RECORD_BYTES: usize = 1024 * 1024;
const DWAL_MAGIC: [u8; 4] = *b"DWAL";
/// magic(4) + seq(8) + payload_len(4).
const DWAL_HEADER_LEN: usize = 16;
/// Smallest possible frame: header + crc32(4).
const DWAL_FRAME_MIN: usize = DWAL_HEADER_LEN + 4;

/// Reflected IEEE 802.3 CRC-32 (poly 0xEDB88320) over every part in order,
/// via the canonical table-driven primitive. `crc32_ieee_update` takes and
/// returns finalized CRCs, so folding the parts in order is identical to
/// hashing their concatenation.
fn crc32_ieee_parts(parts: &[&[u8]]) -> u32 {
    parts.iter().fold(0u32, |crc, part| {
        megu_core::hash::crc32_ieee_update(crc, part)
    })
}

/// Reject `..`, NUL and control characters in operator-controlled data paths so
/// a crafted name can never walk out of the data directory or smuggle control
/// bytes into operator logs.
fn ops_data_path_checked(path: &std::path::Path, what: &str) -> Result<(), String> {
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("{what}: '..' not allowed in {}", path.display()));
        }
    }
    let text = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| format!("{what}: non-UTF-8 path"))?;
    if text.chars().any(|c| c.is_control()) {
        return Err(format!("{what}: control character in path"));
    }
    Ok(())
}

/// One journaled world delta. `CheckpointMarker` is the commit boundary: frames
/// up to and including the newest marker are already covered by a full save and
/// recovery skips them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DeltaWalRecord {
    BlockSet {
        dim: DimId,
        x: i32,
        y: i32,
        z: i32,
        state: u32,
    },
    PlayerSnapshot {
        uuid: [u8; 16],
        dim: DimId,
        pos: [f64; 3],
        hp: f32,
        food: i32,
        xp: i64,
    },
    ContainerSlot {
        dim: DimId,
        x: i32,
        y: i32,
        z: i32,
        slot: i32,
        item_id: i64,
        count: i32,
    },
    CheckpointMarker {
        checkpoint_seq: u64,
    },
}

const DWAL_TAG_BLOCK_SET: u8 = 1;
const DWAL_TAG_PLAYER_SNAPSHOT: u8 = 2;
const DWAL_TAG_CONTAINER_SLOT: u8 = 3;
const DWAL_TAG_CHECKPOINT: u8 = 4;

/// Bounds-checked payload cursor; every read proves the bytes exist first.
struct DwalReader<'a> {
    b: &'a [u8],
}

impl<'a> DwalReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.b.len() < n {
            return Err("truncated record payload".into());
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "invalid u32 in journal frame")?))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "invalid i32 in journal frame")?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "invalid u64 in journal frame")?))
    }
    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "invalid i64 in journal frame")?))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "invalid f32 in journal frame")?))
    }
    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "invalid f64 in journal frame")?))
    }
    fn dim(&mut self) -> Result<DimId, String> {
        Ok(DimId::from_raw(self.u32()?))
    }
    /// Nothing may follow a record: trailing bytes mean a corrupt payload.
    fn done(&self) -> Result<(), String> {
        if self.b.is_empty() {
            Ok(())
        } else {
            Err("trailing bytes after record".into())
        }
    }
}

impl DeltaWalRecord {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        match *self {
            DeltaWalRecord::BlockSet {
                dim,
                x,
                y,
                z,
                state,
            } => {
                out.push(DWAL_TAG_BLOCK_SET);
                out.extend_from_slice(&dim.raw().to_be_bytes());
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
                out.extend_from_slice(&z.to_be_bytes());
                out.extend_from_slice(&state.to_be_bytes());
            }
            DeltaWalRecord::PlayerSnapshot {
                uuid,
                dim,
                pos,
                hp,
                food,
                xp,
            } => {
                out.push(DWAL_TAG_PLAYER_SNAPSHOT);
                out.extend_from_slice(&uuid);
                out.extend_from_slice(&dim.raw().to_be_bytes());
                for v in pos {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                out.extend_from_slice(&hp.to_be_bytes());
                out.extend_from_slice(&food.to_be_bytes());
                out.extend_from_slice(&xp.to_be_bytes());
            }
            DeltaWalRecord::ContainerSlot {
                dim,
                x,
                y,
                z,
                slot,
                item_id,
                count,
            } => {
                out.push(DWAL_TAG_CONTAINER_SLOT);
                out.extend_from_slice(&dim.raw().to_be_bytes());
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
                out.extend_from_slice(&z.to_be_bytes());
                out.extend_from_slice(&slot.to_be_bytes());
                out.extend_from_slice(&item_id.to_be_bytes());
                out.extend_from_slice(&count.to_be_bytes());
            }
            DeltaWalRecord::CheckpointMarker { checkpoint_seq } => {
                out.push(DWAL_TAG_CHECKPOINT);
                out.extend_from_slice(&checkpoint_seq.to_be_bytes());
            }
        }
        out
    }

    fn decode(payload: &[u8]) -> Result<DeltaWalRecord, String> {
        let mut r = DwalReader { b: payload };
        let record = match r.take(1)?[0] {
            DWAL_TAG_BLOCK_SET => DeltaWalRecord::BlockSet {
                dim: r.dim()?,
                x: r.i32()?,
                y: r.i32()?,
                z: r.i32()?,
                state: r.u32()?,
            },
            DWAL_TAG_PLAYER_SNAPSHOT => {
                let uuid: [u8; 16] = r.take(16)?.try_into().map_err(|_| "invalid player UUID in journal frame")?;
                let dim = r.dim()?;
                let pos = [r.f64()?, r.f64()?, r.f64()?];
                DeltaWalRecord::PlayerSnapshot {
                    uuid,
                    dim,
                    pos,
                    hp: r.f32()?,
                    food: r.i32()?,
                    xp: r.i64()?,
                }
            }
            DWAL_TAG_CONTAINER_SLOT => DeltaWalRecord::ContainerSlot {
                dim: r.dim()?,
                x: r.i32()?,
                y: r.i32()?,
                z: r.i32()?,
                slot: r.i32()?,
                item_id: r.i64()?,
                count: r.i32()?,
            },
            DWAL_TAG_CHECKPOINT => DeltaWalRecord::CheckpointMarker {
                checkpoint_seq: r.u64()?,
            },
            tag => return Err(format!("unknown record tag {tag}")),
        };
        r.done()?;
        Ok(record)
    }
}

/// Serialize one frame: magic, seq, len, payload, crc32(seq||len||payload).
fn dwal_frame_bytes(seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DWAL_FRAME_MIN + payload.len());
    out.extend_from_slice(&DWAL_MAGIC);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    let crc = crc32_ieee_parts(&[&out[4..]]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// The verified contents of a WAL file: every intact frame plus how much of a
/// torn tail was cut short. `good_len` is the byte offset just past the last
/// verified frame - where a repair truncation should cut.
struct DwalScan {
    frames: Vec<(u64, DeltaWalRecord)>,
    last_seq: u64,
    good_len: u64,
    truncated_tail_bytes: usize,
}

/// Parse `bytes` frame by frame. A partial trailing frame (fewer bytes than the
/// declared record) is a torn tail; a bad magic, an over-cap length, a CRC
/// mismatch, or an undecodable payload anywhere is corruption and fails closed.
fn dwal_scan(bytes: &[u8]) -> Result<DwalScan, String> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    let mut last_seq = 0u64;
    loop {
        let rest = bytes.len() - offset;
        if rest == 0 {
            break;
        }
        if rest < DWAL_FRAME_MIN {
            break; // torn tail: fewer bytes than the smallest frame
        }
        let head = &bytes[offset..];
        if head[..4] != DWAL_MAGIC {
            return Err(format!("delta WAL: bad frame magic at offset {offset}"));
        }
        let seq = u64::from_be_bytes(head[4..12].try_into().map_err(|_| "invalid journal sequence")?);
        let len = u32::from_be_bytes(head[12..16].try_into().map_err(|_| "invalid journal payload length")?) as usize;
        // The cap is checked BEFORE any allocation sized by `len`.
        if len > MAX_WAL_RECORD_BYTES {
            return Err(format!(
                "delta WAL: payload length {len} over cap at offset {offset}"
            ));
        }
        if rest < DWAL_FRAME_MIN + len {
            break; // torn tail: the declared payload never fully arrived
        }
        let payload = &head[DWAL_HEADER_LEN..DWAL_HEADER_LEN + len];
        let crc_at = DWAL_HEADER_LEN + len;
        let stored_crc = u32::from_be_bytes(head[crc_at..crc_at + 4].try_into().map_err(|_| "invalid journal checksum")?);
        if crc32_ieee_parts(&[&head[4..DWAL_HEADER_LEN], payload]) != stored_crc {
            return Err(format!("delta WAL: crc mismatch at offset {offset}"));
        }
        let record = DeltaWalRecord::decode(payload)
            .map_err(|e| format!("delta WAL: offset {offset}: {e}"))?;
        frames.push((seq, record));
        last_seq = seq;
        offset += DWAL_FRAME_MIN + len;
    }
    Ok(DwalScan {
        frames,
        last_seq,
        good_len: offset as u64,
        truncated_tail_bytes: bytes.len() - offset,
    })
}

/// What `recover_after_checkpoint` learned from the log.
#[derive(Clone, Debug)]
pub(crate) struct WalRecoveryReport {
    /// The newest committed checkpoint's sequence, if one was journaled.
    #[cfg(test)]
    pub(crate) checkpoint_seq: Option<u64>,
    /// Highest frame sequence seen; the journal continues from `+ 1`.
    #[cfg(test)]
    pub(crate) last_committed_seq: u64,
    /// Every intact record strictly newer than the last checkpoint marker.
    pub(crate) records: Vec<DeltaWalRecord>,
    /// Bytes of an incomplete trailing frame skipped at EOF (a crash tore the
    /// last append). All preceding CRC-verified frames were replayed intact.
    pub(crate) truncated_tail_bytes: usize,
}

/// Replay a WAL file: validate the path, verify every frame's CRC, then return
/// the records strictly newer than the newest `CheckpointMarker`. A missing
/// file recovers as empty; a torn tail reports `truncated_tail_bytes`; corrupt
/// magic, length, CRC or payload mid-stream is an `Err`.
pub(crate) fn recover_after_checkpoint(
    path: &std::path::Path,
) -> Result<WalRecoveryReport, String> {
    ops_data_path_checked(path, "delta WAL")?;
    let bytes = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WalRecoveryReport {
                #[cfg(test)]
                checkpoint_seq: None,
                #[cfg(test)]
                last_committed_seq: 0,
                records: Vec::new(),
                truncated_tail_bytes: 0,
            });
        }
        Err(e) => return Err(format!("delta WAL: read {}: {e}", path.display())),
        Ok(b) => b,
    };
    let scan = dwal_scan(&bytes)?;
    #[cfg(test)]
    let mut checkpoint_seq = None;
    let mut checkpoint_frame_seq = 0u64;
    for (seq, record) in &scan.frames {
        if let DeltaWalRecord::CheckpointMarker { checkpoint_seq: _cp } = record {
            #[cfg(test)]
            {
                checkpoint_seq = Some(*_cp);
            }
            checkpoint_frame_seq = *seq;
        }
    }
    let records = scan
        .frames
        .iter()
        .filter(|(seq, _)| *seq > checkpoint_frame_seq)
        .map(|(_, record)| record.clone())
        .collect();
    Ok(WalRecoveryReport {
        #[cfg(test)]
        checkpoint_seq,
        #[cfg(test)]
        last_committed_seq: scan.last_seq,
        records,
        truncated_tail_bytes: scan.truncated_tail_bytes,
    })
}

/// Append-only journal of world deltas between full saves. `next_seq` never
/// resets inside one journal (and `open` continues past the highest sequence on
/// disk), so a sequence number always orders frames on the same timeline.
pub(crate) struct DeltaWalJournal {
    path: std::path::PathBuf,
    file: std::fs::File,
    next_seq: u64,
}

impl DeltaWalJournal {
    /// Open (or create) a WAL. A torn tail left by a crash is truncated first,
    /// so new frames never sit behind bytes recovery would report anyway; a
    /// mid-stream corrupt log fails closed.
    pub(crate) fn open(path: impl AsRef<std::path::Path>) -> Result<DeltaWalJournal, String> {
        let path = path.as_ref().to_path_buf();
        ops_data_path_checked(&path, "delta WAL")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("delta WAL: create {}: {e}", parent.display()))?;
            }
        }
        let mut next_seq = 1u64;
        if path.exists() {
            let existing = std::fs::read(&path)
                .map_err(|e| format!("delta WAL: read {}: {e}", path.display()))?;
            if !existing.is_empty() {
                let scan = dwal_scan(&existing)?;
                if scan.truncated_tail_bytes > 0 {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .and_then(|f| f.set_len(scan.good_len))
                        .map_err(|e| format!("delta WAL: truncate torn tail: {e}"))?;
                }
                next_seq = scan.last_seq + 1;
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("delta WAL: open {}: {e}", path.display()))?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0))
            .map_err(|e| format!("delta WAL: seek: {e}"))?;
        Ok(DeltaWalJournal {
            path,
            file,
            next_seq,
        })
    }

    /// The sequence the next appended frame will get.
    pub(crate) fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Frame, append and `sync_data`: on return the record survives a crash, so
    /// data loss between full saves is bounded by the append rate, not the save
    /// interval.
    #[cfg(test)]
    pub(crate) fn append_and_flush(&mut self, record: &DeltaWalRecord) -> Result<u64, String> {
        let payload = record.encode();
        if payload.len() > MAX_WAL_RECORD_BYTES {
            return Err(format!(
                "delta WAL: record payload {} over cap",
                payload.len()
            ));
        }
        let seq = self.next_seq;
        self.file
            .write_all(&dwal_frame_bytes(seq, &payload))
            .and_then(|_| self.file.sync_data())
            .map_err(|e| format!("delta WAL: append {}: {e}", self.path.display()))?;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Convenience for journaling a checkpoint boundary.
    #[cfg(test)]
    pub(crate) fn append_checkpoint(&mut self, checkpoint_seq: u64) -> Result<u64, String> {
        self.append_and_flush(&DeltaWalRecord::CheckpointMarker { checkpoint_seq })
    }

    /// After a full save succeeded, shrink the WAL to a single fresh checkpoint
    /// marker: write the anchor frame to a sibling file, sync it, and rename it
    /// over the log, so a crash leaves either the old WAL or the new anchor -
    /// never a half-truncated file. The sequence counter keeps running so frame
    /// order stays monotonic across the rotation.
    pub(crate) fn compact_after_checkpoint(&mut self, checkpoint_seq: u64) -> Result<(), String> {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".compact-tmp");
        let tmp = self.path.with_file_name(name);
        let seq = self.next_seq;
        let frame = dwal_frame_bytes(
            seq,
            &DeltaWalRecord::CheckpointMarker { checkpoint_seq }.encode(),
        );
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| format!("delta WAL: compact tmp {}: {e}", tmp.display()))?;
            let written = file.write_all(&frame).and_then(|_| file.sync_all());
            drop(file);
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("delta WAL: compact write: {e}"));
            }
        }
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("delta WAL: compact rename: {e}"))?;
        self.file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| format!("delta WAL: reopen {}: {e}", self.path.display()))?;
        std::io::Seek::seek(&mut self.file, std::io::SeekFrom::End(0))
            .map_err(|e| format!("delta WAL: seek: {e}"))?;
        self.next_seq = seq + 1;
        Ok(())
    }
}

// ================= ticking-crash quarantine =================
//
// A block entity, entity or chunk whose tick panics used to take the tick
// thread (and the world) down with it. `run_quarantined_tick` runs the tick
// under `catch_unwind`: after `max_consecutive_faults` panics (default 1 for a
// critical tick panic) only that target is frozen and skipped on later ticks,
// with a plain-language diagnostic for operators. The world stays online and
// uncorrupted; releasing a frozen target requires op level 4, and the frozen
// set can be persisted so a crash-looping target stays frozen after restart.

/// What one frozen tick target is. `kind`/`eid` are for the operator
/// diagnostic; matching for `is_quarantined`/`unfreeze` is the whole value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg(test)]
pub(crate) enum QuarantineTarget {
    BlockEntity {
        dim: DimId,
        x: i32,
        y: i32,
        z: i32,
        kind: String,
    },
    Entity {
        dim: DimId,
        eid: i32,
        kind: String,
    },
    Chunk {
        dim: DimId,
        cx: i32,
        cz: i32,
    },
}

#[cfg(test)]
impl QuarantineTarget {
    /// Plain-language identity for operator logs and diagnostics.
    fn describe(&self) -> String {
        match self {
            QuarantineTarget::BlockEntity { dim, x, y, z, kind } => format!(
                "block entity {kind} at ({x},{y},{z}) in dimension {}",
                dim.raw()
            ),
            QuarantineTarget::Entity { dim, eid, kind } => {
                format!("entity {kind} (eid {eid}) in dimension {}", dim.raw())
            }
            QuarantineTarget::Chunk { dim, cx, cz } => {
                format!("chunk ({cx},{cz}) in dimension {}", dim.raw())
            }
        }
    }
}

#[derive(Clone, Debug)]
#[cfg(test)]
struct QuarantineRecord {
    consecutive_faults: u32,
    quarantined: bool,
    /// One sanitized line: target description + bounded panic reason.
    diagnostic: String,
}

/// The frozen set plus per-target fault counters. Interior locking so the tick
/// loop can share one instance across workers.
#[cfg(test)]
pub(crate) struct TickFaultQuarantine {
    /// Faults before a target freezes; 1 for critical tick panics.
    max_consecutive_faults: u32,
    records: Mutex<HashMap<QuarantineTarget, QuarantineRecord>>,
}

#[cfg(test)]
impl TickFaultQuarantine {
    pub(crate) fn new(max_consecutive_faults: u32) -> Self {
        TickFaultQuarantine {
            max_consecutive_faults: max_consecutive_faults.max(1),
            records: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn is_quarantined(&self, target: &QuarantineTarget) -> bool {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(target)
            .is_some_and(|r| r.quarantined)
    }

    /// The operator-facing line recorded for a target's last fault.
    pub(crate) fn diagnostic(&self, target: &QuarantineTarget) -> Option<String> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(target)
            .map(|r| r.diagnostic.clone())
    }

    /// Every currently frozen target, for `/ops` surfaces and persistence.
    pub(crate) fn quarantined(&self) -> Vec<QuarantineTarget> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, r)| r.quarantined)
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Release a frozen target. Releasing a crash source is an operator-level
    /// decision, so anything below op level 4 is refused outright.
    pub(crate) fn unfreeze(
        &self,
        target: &QuarantineTarget,
        actor_op_level: u8,
    ) -> Result<bool, String> {
        if actor_op_level < 4 {
            return Err(format!(
                "refused: releasing quarantined {} requires op level 4 (got {actor_op_level})",
                target.describe()
            ));
        }
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if records.get(target).is_some_and(|r| r.quarantined) {
            records.remove(target);
            return Ok(true);
        }
        Ok(false)
    }

    /// Persist the frozen set so a crash-looping target stays frozen across a
    /// restart. One sanitized line per target; atomic temp+sync+rename write.
    pub(crate) fn persist(&self, path: &std::path::Path) -> Result<(), String> {
        ops_data_path_checked(path, "quarantine evidence")?;
        let mut out = String::from(QUARANTINE_FILE_HEADER);
        out.push('\n');
        for (target, record) in self.records.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if !record.quarantined {
                continue;
            }
            out.push_str(&target.persist_line(record));
            out.push('\n');
        }
        write_bytes_atomic(path, out.as_bytes())
            .map_err(|e| format!("quarantine persist {}: {e}", path.display()))
    }

    /// Load a frozen set written by `persist`. A missing file is a fresh,
    /// empty quarantine; malformed lines are skipped.
    pub(crate) fn load_persisted(
        path: &std::path::Path,
        max_consecutive_faults: u32,
    ) -> Result<TickFaultQuarantine, String> {
        ops_data_path_checked(path, "quarantine evidence")?;
        let text = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TickFaultQuarantine::new(max_consecutive_faults));
            }
            Err(e) => return Err(format!("quarantine load {}: {e}", path.display())),
            Ok(t) => t,
        };
        let quarantine = TickFaultQuarantine::new(max_consecutive_faults);
        for line in text.lines() {
            if let Some((target, faults, diagnostic)) = parse_quarantine_line(line) {
                quarantine.records.lock().unwrap_or_else(|e| e.into_inner()).insert(
                    target,
                    QuarantineRecord {
                        consecutive_faults: faults,
                        quarantined: true,
                        diagnostic,
                    },
                );
            }
        }
        Ok(quarantine)
    }
}

#[cfg(test)]
impl Default for TickFaultQuarantine {
    fn default() -> Self {
        // A critical tick panic freezes its target on first fault.
        TickFaultQuarantine::new(1)
    }
}

#[cfg(test)]
const QUARANTINE_FILE_HEADER: &str = "MEGU-QUARANTINE-V1";

/// One line for the persisted frozen set. Control characters in `kind` and the
/// diagnostic become spaces, so a field can never smuggle extra lines or
/// terminal escapes into an operator file.
#[cfg(test)]
fn quarantine_field(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
impl QuarantineTarget {
    fn persist_line(&self, record: &QuarantineRecord) -> String {
        let faults = record.consecutive_faults;
        let diag = quarantine_field(&record.diagnostic);
        match self {
            QuarantineTarget::BlockEntity { dim, x, y, z, kind } => format!(
                "be\t{}\t{x}\t{y}\t{z}\t{faults}\t{}\t{diag}",
                dim.raw(),
                quarantine_field(kind)
            ),
            QuarantineTarget::Entity { dim, eid, kind } => format!(
                "ent\t{}\t{eid}\t{faults}\t{}\t{diag}",
                dim.raw(),
                quarantine_field(kind)
            ),
            QuarantineTarget::Chunk { dim, cx, cz } => {
                format!("chunk\t{}\t{cx}\t{cz}\t{faults}\t{diag}", dim.raw())
            }
        }
    }
}

#[cfg(test)]
fn parse_quarantine_line(line: &str) -> Option<(QuarantineTarget, u32, String)> {
    let p: Vec<&str> = line.split('\t').collect();
    let num = |s: &&str| s.parse::<i64>().ok();
    match p.as_slice() {
        ["be", dim, x, y, z, faults, kind, diag @ ..] => {
            let target = QuarantineTarget::BlockEntity {
                dim: DimId::from_raw(num(dim)? as u32),
                x: num(x)? as i32,
                y: num(y)? as i32,
                z: num(z)? as i32,
                kind: kind.to_string(),
            };
            Some((target, num(faults)? as u32, diag.join("\t")))
        }
        ["ent", dim, eid, faults, kind, diag @ ..] => {
            let target = QuarantineTarget::Entity {
                dim: DimId::from_raw(num(dim)? as u32),
                eid: num(eid)? as i32,
                kind: kind.to_string(),
            };
            Some((target, num(faults)? as u32, diag.join("\t")))
        }
        ["chunk", dim, cx, cz, faults, diag @ ..] => {
            let target = QuarantineTarget::Chunk {
                dim: DimId::from_raw(num(dim)? as u32),
                cx: num(cx)? as i32,
                cz: num(cz)? as i32,
            };
            Some((target, num(faults)? as u32, diag.join("\t")))
        }
        _ => None,
    }
}

/// What became of one `run_quarantined_tick` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub(crate) enum TickFaultOutcome {
    /// The tick ran and finished.
    Ticked,
    /// The target is frozen; the closure never ran.
    SkippedQuarantined,
    /// The tick panicked; `quarantined` reports whether this fault froze it.
    Faulted { quarantined: bool },
}

/// A panic message made safe for an operator file: printable characters only,
/// whitespace collapsed, bounded length. No stack, no raw payload object, no
/// secrets - one line a log page can show verbatim.
#[cfg(test)]
fn quarantine_sanitize(message: &str) -> String {
    let printable: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::with_capacity(240);
    for word in printable.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= 240 {
            break;
        }
    }
    let out: String = out.chars().take(240).collect();
    if out.is_empty() {
        "panic without message".into()
    } else {
        out
    }
}

/// Run one tick for `target`. A frozen target is skipped without calling `f`;
/// a panicking `f` is caught - the tick thread never sees it - counted, and
/// past `max_consecutive_faults` the target is frozen with a sanitized
/// diagnostic. A clean tick resets the consecutive-fault counter.
#[cfg(test)]
pub(crate) fn run_quarantined_tick(
    quarantine: &TickFaultQuarantine,
    target: QuarantineTarget,
    f: impl FnOnce() + std::panic::UnwindSafe,
) -> TickFaultOutcome {
    {
        let records = quarantine.records.lock().unwrap_or_else(|e| e.into_inner());
        if records.get(&target).is_some_and(|r| r.quarantined) {
            return TickFaultOutcome::SkippedQuarantined;
        }
    }
    let panic = match std::panic::catch_unwind(f) {
        Ok(()) => None,
        Err(payload) => Some(panic_payload_message(&payload)),
    };
    let Some(message) = panic else {
        let mut records = quarantine.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = records.get_mut(&target) {
            record.consecutive_faults = 0;
        }
        return TickFaultOutcome::Ticked;
    };
    let message = quarantine_sanitize(&message);
    let mut records = quarantine.records.lock().unwrap_or_else(|e| e.into_inner());
    let record = records
        .entry(target.clone())
        .or_insert_with(|| QuarantineRecord {
            consecutive_faults: 0,
            quarantined: false,
            diagnostic: String::new(),
        });
    record.consecutive_faults = record.consecutive_faults.saturating_add(1);
    if record.consecutive_faults >= quarantine.max_consecutive_faults {
        record.quarantined = true;
    }
    record.diagnostic = format!(
        "{} faulted tick {}/{}: {}",
        target.describe(),
        record.consecutive_faults,
        quarantine.max_consecutive_faults,
        message
    );
    let quarantined = record.quarantined;
    if quarantined {
        eprintln!("[quarantine] froze {}: {}", target.describe(), message);
    }
    TickFaultOutcome::Faulted { quarantined }
}

// ================= per-chunk lag hotspot quarantine =================
//
// Paper, Purpur and NeoForge can name a lagging plugin or a lagging entity;
// none of them can say "chunk (10,-4) in the_nether burned 18 ms of the tick
// for 40 ticks straight, so we throttled just that chunk while every other
// player kept 20 TPS". `ChunkHotspotQuarantine` is that missing piece: the
// tick loop reports per-(dimension, chunk) cost samples through
// `record_chunk_cost_us`, a per-cost-kind EWMA smooths them for the hotspot
// report, and a chunk whose *whole-tick* cost stays over
// `quarantine_threshold_us` for `trigger_ticks` consecutive ticks drops to a
// 1-in-`cadence` tick cadence — a trickle, not a freeze, so its block
// entities still smelt. Hysteresis lifts the throttle only after
// `hysteresis_exit_ticks` consecutive ticks under `hysteresis_exit_us`, or an
// operator clears it by hand. The tracked set is hard-bounded and evicts the
// least-recently-active entries, so the tracker itself can never become the
// leak it exists to find. Nothing here reads a wall clock: `tick` is the
// caller's monotonic tick counter.

/// Which subsystem inside one chunk burned the tick budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ChunkCostKind {
    /// Ticking tile entities: hoppers, droppers, furnaces, chests, ...
    BlockEntity,
    /// Mob/item entity AI and movement inside the chunk.
    Entities,
    /// Redstone wire/piston/repeater propagation.
    Redstone,
    /// Mod-driven machines — the runaway-mod case no vanilla profiler names.
    ModMachine,
}

impl ChunkCostKind {
    const ALL: [ChunkCostKind; 4] = [
        Self::BlockEntity,
        Self::Entities,
        Self::Redstone,
        Self::ModMachine,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Stable operator-facing spelling for reports and alerts.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BlockEntity => "block_entity",
            Self::Entities => "entities",
            Self::Redstone => "redstone",
            Self::ModMachine => "mod_machine",
        }
    }
}

/// `(dimension, chunk x, chunk z)` — the identity a hotspot is quarantined by.
type ChunkHotspotKey = (DimId, i32, i32);

/// Tunables for `ChunkHotspotQuarantine`; `Default` is the shipped policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkHotspotPolicy {
    /// Smoothed per-chunk tick cost (µs) that counts as "hot".
    pub quarantine_threshold_us: u64,
    /// Smoothed cost that must hold for `hysteresis_exit_ticks` to lift.
    pub hysteresis_exit_us: u64,
    /// Consecutive hot ticks before the chunk drops to the slow cadence.
    pub trigger_ticks: u32,
    /// Consecutive cool ticks that auto-lift a quarantine.
    pub hysteresis_exit_ticks: u32,
    /// A quarantined chunk ticks once every this many ticks.
    pub cadence: u64,
}

impl Default for ChunkHotspotPolicy {
    fn default() -> Self {
        Self {
            quarantine_threshold_us: 15_000,
            hysteresis_exit_us: 4_000,
            trigger_ticks: 3,
            hysteresis_exit_ticks: 20,
            cadence: 8,
        }
    }
}

/// Hard bound on tracked chunks; least-recently-active entries evict first
/// so the tracker itself stays a fixed-size structure.
const CHUNK_HOTSPOT_MAX_TRACKED: usize = 4_096;
/// EWMA weight of the newest cost sample.
const CHUNK_HOTSPOT_EWMA_ALPHA: f64 = 0.4;
/// Operator alerts kept for `/megu status`-style surfaces.
const CHUNK_HOTSPOT_MAX_ALERTS: usize = 64;

#[derive(Default)]
struct ChunkHotspotEntry {
    /// Per-kind EWMA in microseconds; `kind_seen` marks which slots hold a
    /// real first sample (an EWMA initialises to the first cost, not zero,
    /// so one 20 ms spike is not diluted by empty history).
    kind_ewma: [f64; 4],
    kind_seen: u8,
    samples: u64,
    /// Tick of the newest sample; the first sample of a *new* tick is also
    /// the boundary that scores the tick before it.
    last_tick: Option<u64>,
    /// Whole-tick cost accumulator: every cost-kind sample of the current
    /// tick sums here and is scored once, when the next tick's first sample
    /// lands. Streaks count *this* instantaneous per-tick total — the spec's
    /// "exceeds the threshold for N consecutive ticks" — while the EWMA
    /// above stays the smoothed metric reports and alerts quote.
    tick_cost_us: f64,
    over_streak: u32,
    cool_streak: u32,
    quarantined: bool,
    /// `tick % cadence` slot this chunk owns while quarantined, so throttled
    /// chunks spread across the tick instead of all firing together.
    phase: u64,
}

/// What changed when a completed tick was scored.
enum HotspotTransition {
    /// `trigger_ticks` consecutive hot ticks elapsed: throttle engaged.
    Quarantined { ewma_us: f64, kind: ChunkCostKind },
    /// `hysteresis_exit_ticks` consecutive cool ticks elapsed: auto-lifted.
    Lifted { ewma_us: f64 },
}

impl ChunkHotspotEntry {
    /// Smoothed total tick cost across every reported cost kind.
    fn total_ewma_us(&self) -> f64 {
        self.kind_ewma.iter().sum()
    }

    /// The cost kind with the highest smoothed share — what the report names.
    fn dominant_kind(&self) -> ChunkCostKind {
        ChunkCostKind::ALL
            .iter()
            .copied()
            .filter(|k| self.kind_seen & (1 << k.index()) != 0)
            .max_by(|a, b| {
                self.kind_ewma[a.index()]
                    .total_cmp(&self.kind_ewma[b.index()])
            })
            .unwrap_or(ChunkCostKind::BlockEntity)
    }

    /// Score the tick that just completed. Called once per tick — on the
    /// first sample of the *next* tick — so multi-kind chunks are judged on
    /// their whole-tick cost, never on one subsystem's partial view.
    fn score_completed_tick(
        &mut self,
        policy: &ChunkHotspotPolicy,
        tick: u64,
        tick_cost_us: f64,
    ) -> Option<HotspotTransition> {
        if tick_cost_us > policy.quarantine_threshold_us as f64 {
            self.over_streak = self.over_streak.saturating_add(1);
            self.cool_streak = 0;
        } else if tick_cost_us < policy.hysteresis_exit_us as f64 {
            self.cool_streak = self.cool_streak.saturating_add(1);
            self.over_streak = 0;
        } else {
            // Inside the hysteresis band the chunk is neither hot enough to
            // count toward a quarantine nor cool enough to earn a lift.
            self.over_streak = 0;
            self.cool_streak = 0;
        }
        if !self.quarantined && self.over_streak >= policy.trigger_ticks {
            self.quarantined = true;
            self.phase = tick % policy.cadence.max(1);
            return Some(HotspotTransition::Quarantined {
                ewma_us: self.total_ewma_us(),
                kind: self.dominant_kind(),
            });
        }
        if self.quarantined && self.cool_streak >= policy.hysteresis_exit_ticks {
            self.quarantined = false;
            return Some(HotspotTransition::Lifted {
                ewma_us: self.total_ewma_us(),
            });
        }
        None
    }
}

/// One row of `top_lag_hotspots` / `format_hotspot_report`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChunkHotspotSummary {
    pub dim: DimId,
    pub cx: i32,
    pub cz: i32,
    /// The subsystem with the highest smoothed share of the chunk's cost.
    pub dominant_kind: ChunkCostKind,
    /// Smoothed whole-chunk tick cost in microseconds.
    pub ewma_us: u64,
    pub samples: u64,
    pub quarantined: bool,
}

#[derive(Default)]
struct ChunkHotspotState {
    chunks: HashMap<ChunkHotspotKey, ChunkHotspotEntry>,
    /// Bounded operator alert ring: quarantine engage/lift/clear lines,
    /// oldest dropped first.
    alerts: Vec<String>,
}

/// Per-(dimension, chunk) lag accounting with automatic throttling. See the
/// section comment for the policy; the short version is: sustained hot
/// chunks drop to a 1-in-`cadence` tick cadence and recover on hysteresis.
pub(crate) struct ChunkHotspotQuarantine {
    policy: ChunkHotspotPolicy,
    state: Mutex<ChunkHotspotState>,
}

impl ChunkHotspotQuarantine {
    pub(crate) fn new() -> Self {
        Self::with_policy(ChunkHotspotPolicy::default())
    }

    pub(crate) fn with_policy(policy: ChunkHotspotPolicy) -> Self {
        ChunkHotspotQuarantine {
            policy,
            state: Mutex::new(ChunkHotspotState::default()),
        }
    }

    /// Report one chunk-subsystem cost sample for `tick`. The tick loop calls
    /// this once per (chunk, cost kind) per tick actually spent; the tracker
    /// itself decides when the chunk is hot enough to throttle.
    pub(crate) fn record_chunk_cost_us(
        &self,
        dim: DimId,
        cx: i32,
        cz: i32,
        kind: ChunkCostKind,
        cost_us: u64,
        tick: u64,
    ) {
        let key = (dim, cx, cz);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ChunkHotspotState { chunks, alerts } = &mut *state;
        if chunks.len() >= CHUNK_HOTSPOT_MAX_TRACKED && !chunks.contains_key(&key) {
            evict_oldest_chunk_entry(chunks);
        }
        let entry = chunks.entry(key).or_default();
        // The first sample of a NEW tick closes the previous tick's books:
        // score its whole-tick cost exactly as that tick left it, once per
        // tick however many cost kinds reported inside it.
        if let Some(prev_tick) = entry.last_tick.filter(|&t| t != tick) {
            let completed_cost = std::mem::take(&mut entry.tick_cost_us);
            if let Some(transition) =
                entry.score_completed_tick(&self.policy, prev_tick, completed_cost)
            {
                let line = match transition {
                    HotspotTransition::Quarantined { ewma_us, kind } => format!(
                        "[hotspot] quarantined dim={} chunk=({cx},{cz}) kind={} \
                         ewma={ewma_us:.0}us cadence=1/{}",
                        chunk_dim_label(dim),
                        kind.as_str(),
                        self.policy.cadence
                    ),
                    HotspotTransition::Lifted { ewma_us } => format!(
                        "[hotspot] auto-lifted dim={} chunk=({cx},{cz}) ewma={ewma_us:.0}us",
                        chunk_dim_label(dim)
                    ),
                };
                push_chunk_alert(alerts, line);
            }
        }
        entry.last_tick = Some(tick);
        entry.tick_cost_us += cost_us as f64;
        let slot = kind.index();
        let cost = cost_us as f64;
        entry.kind_ewma[slot] = if entry.kind_seen & (1 << slot) != 0 {
            CHUNK_HOTSPOT_EWMA_ALPHA * cost
                + (1.0 - CHUNK_HOTSPOT_EWMA_ALPHA) * entry.kind_ewma[slot]
        } else {
            cost
        };
        entry.kind_seen |= 1 << slot;
        entry.samples = entry.samples.saturating_add(1);
    }

    /// Whether the chunk's tick work may run on `tick`. Healthy chunks always
    /// may; a quarantined chunk owns exactly one `tick % cadence == phase`
    /// slot per cadence window.
    pub(crate) fn chunk_due(&self, dim: DimId, cx: i32, cz: i32, tick: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.chunks.get(&(dim, cx, cz)) {
            Some(entry) if entry.quarantined => {
                tick % self.policy.cadence.max(1) == entry.phase
            }
            _ => true,
        }
    }

    /// Whether the chunk is currently on the throttled cadence.
    #[cfg(test)]
    pub(crate) fn is_chunk_quarantined(&self, dim: DimId, cx: i32, cz: i32) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get(&(dim, cx, cz))
            .is_some_and(|entry| entry.quarantined)
    }

    /// Operator override: release a quarantined chunk now. Returns whether a
    /// quarantine was actually lifted.
    #[cfg(test)]
    pub(crate) fn clear_chunk_quarantine(&self, dim: DimId, cx: i32, cz: i32) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ChunkHotspotState { chunks, alerts } = &mut *state;
        let Some(entry) = chunks.get_mut(&(dim, cx, cz)) else {
            return false;
        };
        if !entry.quarantined {
            return false;
        }
        entry.quarantined = false;
        entry.over_streak = 0;
        entry.cool_streak = 0;
        push_chunk_alert(
            alerts,
            format!(
                "[hotspot] quarantine cleared by operator dim={} chunk=({cx},{cz})",
                chunk_dim_label(dim)
            ),
        );
        true
    }

    /// The operator alert ring, oldest first — quarantine engage/lift/clear
    /// lines a `/megu status` surface can show verbatim.
    #[cfg(test)]
    pub(crate) fn alerts(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alerts
            .clone()
    }

    /// Hottest chunks by smoothed tick cost, worst first. Ties break on
    /// `(dim, cx, cz)` so the ordering is deterministic for reports.
    #[cfg(test)]
    pub(crate) fn top_lag_hotspots(&self, limit: usize) -> Vec<ChunkHotspotSummary> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = chunk_hotspot_summaries(&state.chunks);
        out.truncate(limit);
        out
    }

    /// Spark-free instant lag diagnosis for `/megu status`: the policy in
    /// force, the tracked/quarantined counts, the top hotspots and the recent
    /// operator alerts — all from in-memory counters, no profiler attach.
    pub(crate) fn format_hotspot_report(&self) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let quarantined = state
            .chunks
            .values()
            .filter(|entry| entry.quarantined)
            .count();
        let mut out = format!(
            "chunk hotspot report: {} chunks tracked, {quarantined} quarantined\n\
             policy: quarantine >{}us for {} ticks, lift <{}us for {} ticks, \
             cadence 1/{}",
            state.chunks.len(),
            self.policy.quarantine_threshold_us,
            self.policy.trigger_ticks,
            self.policy.hysteresis_exit_us,
            self.policy.hysteresis_exit_ticks,
            self.policy.cadence,
        );
        let mut top = chunk_hotspot_summaries(&state.chunks);
        top.truncate(10);
        if top.is_empty() {
            out.push_str("\nno chunk cost samples recorded");
        }
        for (i, s) in top.iter().enumerate() {
            out.push_str(&format!(
                "\n#{:<2} dim={} chunk=({}, {}) kind={} ewma={}us samples={}{}",
                i + 1,
                chunk_dim_label(s.dim),
                s.cx,
                s.cz,
                s.dominant_kind.as_str(),
                s.ewma_us,
                s.samples,
                if s.quarantined { " QUARANTINED" } else { "" },
            ));
        }
        if !state.alerts.is_empty() {
            out.push_str("\nrecent alerts:");
            for alert in state.alerts.iter().rev().take(8) {
                out.push('\n');
                out.push_str(alert);
            }
        }
        out
    }
}

impl Default for ChunkHotspotQuarantine {
    fn default() -> Self {
        Self::new()
    }
}

/// Sorted hotspot rows shared by `top_lag_hotspots` and the report so both
/// see one ordering under a single lock.
fn chunk_hotspot_summaries(
    chunks: &HashMap<ChunkHotspotKey, ChunkHotspotEntry>,
) -> Vec<ChunkHotspotSummary> {
    let mut out: Vec<ChunkHotspotSummary> = chunks
        .iter()
        .map(|(&(dim, cx, cz), entry)| ChunkHotspotSummary {
            dim,
            cx,
            cz,
            dominant_kind: entry.dominant_kind(),
            ewma_us: entry.total_ewma_us().round().max(0.0) as u64,
            samples: entry.samples,
            quarantined: entry.quarantined,
        })
        .collect();
    out.sort_by(|a, b| {
        b.ewma_us
            .cmp(&a.ewma_us)
            .then(a.dim.cmp(&b.dim))
            .then(a.cx.cmp(&b.cx))
            .then(a.cz.cmp(&b.cz))
    });
    out
}

/// Drop the least-recently-active entry. Quarantined chunks are only evicted
/// when every tracked chunk is quarantined — the hard bound wins over the
/// freeze, but only as the last resort.
fn evict_oldest_chunk_entry(chunks: &mut HashMap<ChunkHotspotKey, ChunkHotspotEntry>) {
    let victim = chunks
        .iter()
        .min_by_key(|(key, entry)| {
            (
                entry.quarantined,
                entry.last_tick.unwrap_or(0),
                key.0,
                key.1,
                key.2,
            )
        })
        .map(|(key, _)| *key);
    if let Some(key) = victim {
        chunks.remove(&key);
    }
}

fn push_chunk_alert(alerts: &mut Vec<String>, line: String) {
    if alerts.len() >= CHUNK_HOTSPOT_MAX_ALERTS {
        alerts.remove(0);
    }
    alerts.push(line);
}

/// Operator-facing dimension label: the canonical id when the slot resolves,
/// a raw-slot fallback when it does not (a datapack dimension mid-reload).
fn chunk_dim_label(dim: DimId) -> String {
    let id = dim.dimension_id();
    if id.is_empty() {
        format!("dim#{}", dim.raw())
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod server_reliability_tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// FIPS-197 Appendix B vector for the block cipher, plus a CFB8 round-trip
    /// across a message longer than one feedback register.
    #[test]
    fn aes128_fips197_vector_and_cfb8_stream_roundtrip() {
        let key: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
            .try_into()
            .unwrap();
        let block: [u8; 16] = unhex("00112233445566778899aabbccddeeff")
            .try_into()
            .unwrap();
        assert_eq!(
            hex(&aes128_encrypt_block(&aes128_expand_key(&key), &block)),
            "69c4e0d86a7b0430d8cdb78070b4c55a"
        );

        // CFB8: Minecraft's key == IV == shared secret; stream across 200 bytes.
        let secret: [u8; 16] = unhex("8e8e9d1a5df24b7a9b3b8e31f0a4c2d5")
            .try_into()
            .unwrap();
        let mut enc = Aes128Cfb8::new(&secret, &secret);
        let mut dec = Aes128Cfb8::new(&secret, &secret);
        let mut msg: Vec<u8> = (0..200u32)
            .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
            .collect();
        let plain = msg.clone();
        // Split the encrypt stream at an odd boundary to prove byte-at-a-time
        // feedback keeps state exactly.
        enc.encrypt(&mut msg[..37]);
        enc.encrypt(&mut msg[37..]);
        assert_ne!(msg, plain);
        dec.decrypt(&mut msg);
        assert_eq!(msg, plain);

        // Encryptor state and decryptor state are independent halves of a pair.
        let mut a = Aes128Cfb8::new(&secret, &secret);
        let mut b = Aes128Cfb8::new(&secret, &secret);
        let mut ping = *b"hello cipher";
        a.encrypt(&mut ping);
        b.decrypt(&mut ping);
        assert_eq!(&ping, b"hello cipher");
    }

    /// wiki.vg / vanilla `BigInteger(digest).toString(16)` vectors, including the
    /// negative two's-complement case.
    #[test]
    fn minecraft_sha1_server_hash_matches_official_positive_and_negative_vectors() {
        assert_eq!(
            mc_server_hash("Notch", &[], &[]),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
        assert_eq!(
            mc_server_hash("jeb_", &[], &[]),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
        assert_eq!(
            mc_server_hash("simon", &[], &[]),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
        // The real login hash concatenates id || secret || key DER.
        assert_eq!(
            mc_server_hash("", &[0x01, 0x02], &[0xaa, 0xbb]),
            mc_signed_hex_digest(&sha1(&[0x01, 0x02, 0xaa, 0xbb]))
        );
    }

    /// Velocity `velocity:player_info` verification: valid HMAC-SHA256 payload
    /// decodes, a flipped byte or wrong secret rejects.
    #[test]
    fn velocity_modern_forwarding_hmac_sha256_verifies_valid_and_rejects_tampered() {
        // RFC 4231 HMAC-SHA256 cases pin the MAC itself before any packet logic.
        assert_eq!(
            hmac_sha256(&[0x0b; 20], b"Hi There"),
            unhex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")[..]
        );
        assert_eq!(
            hmac_sha256(b"Jefe", b"what do ya want for nothing?"),
            unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")[..]
        );

        let secret = b"forwarding-secret!";
        let uuid: [u8; 16] = unhex("00112233445566778899aabbccddeeff")
            .try_into()
            .unwrap();
        let mut body = Vec::new();
        write_varint(&mut body, 4); // forwarding version
        write_str(&mut body, "203.0.113.7");
        body.extend_from_slice(&uuid);
        write_str(&mut body, "Steve");
        write_varint(&mut body, 1); // one property
        write_str(&mut body, "textures");
        write_str(&mut body, "aGVsbG8=");
        body.push(1);
        write_str(&mut body, "c2ln");
        let mut payload = hmac_sha256(secret, &body).to_vec();
        payload.extend_from_slice(&body);

        let player = verify_velocity_forwarding(secret, &payload).unwrap();
        assert_eq!(player.version, 4);
        assert_eq!(player.forwarded_ip, "203.0.113.7");
        assert_eq!(player.uuid, uuid);
        assert_eq!(player.username, "Steve");
        assert_eq!(
            player.properties,
            vec![ForwardedProperty {
                name: "textures".into(),
                value: "aGVsbG8=".into(),
                signature: Some("c2ln".into()),
            }]
        );

        // Any tamper - one flipped byte anywhere in the body - must reject.
        for pos in [0, 32, 40, payload.len() - 1] {
            let mut bad = payload.clone();
            bad[pos] ^= 0x80;
            assert!(
                verify_velocity_forwarding(secret, &bad).is_err(),
                "tamper at {pos} accepted"
            );
        }
        assert!(verify_velocity_forwarding(b"wrong-secret", &payload).is_err());
        assert!(verify_velocity_forwarding(secret, &payload[..40]).is_err());
        assert!(verify_velocity_forwarding(&[], &payload).is_err());

        // An out-of-range version with a *valid* tag rejects after the MAC passes.
        let mut v5 = Vec::new();
        write_varint(&mut v5, 5);
        let mut tagged = hmac_sha256(secret, &v5).to_vec();
        tagged.extend_from_slice(&v5);
        assert!(verify_velocity_forwarding(secret, &tagged).is_err());
    }

    /// Round-trip the login encryption codecs, and prove a panicking handler is
    /// a counted, cleaned-up disconnect rather than a crash.
    #[test]
    fn connection_crash_isolation_catches_panic_and_runs_cleanup() {
        // `ISOLATED_CONNECTION_DISCONNECTS` is asserted as `before + 1` below
        // and by `w4_isolated_connection_*` — concurrent runs break both.
        let _guard = crate::test_support::global_world_test_guard();
        let req = encode_encryption_request("", &[0x30; 5], &[9, 8, 7, 6], true);
        assert_eq!(
            decode_encryption_request(&req),
            Some(("".to_string(), vec![0x30; 5], vec![9, 8, 7, 6], true))
        );
        let resp = encode_encryption_response(&[1; 128], &[2; 128]);
        assert_eq!(
            decode_encryption_response(&resp),
            Some((vec![1; 128], vec![2; 128]))
        );
        assert!(decode_encryption_response(&resp[..resp.len() - 1]).is_none());

        let before = ISOLATED_CONNECTION_DISCONNECTS.load(Ordering::SeqCst);
        let cleaned = AtomicU64::new(0);
        let out: Option<u8> = run_isolated_connection(
            "203.0.113.9",
            || -> u8 { panic!("synthetic handler bug") },
            || {
                cleaned.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(out, None);
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(
            ISOLATED_CONNECTION_DISCONNECTS.load(Ordering::SeqCst),
            before + 1
        );

        // A handler that returns normally runs no cleanup and yields its value.
        let ok = run_isolated_connection("fine", || 7u8, || panic!("cleanup on success"));
        assert_eq!(ok, Some(7));
    }

    /// The worker coalesces queued snapshots while the disk is busy, writes the
    /// latest atomically, drains on drop, and backup rotation prunes to `max_keep`.
    #[test]
    fn async_save_worker_coalesces_writes_and_rotates_backups_cleanly() {
        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-reliability-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let worker = AsyncSaveWorker::spawn();
        worker.set_paused(true);
        let a: std::sync::Arc<[u8]> = b"snapshot-a".as_slice().into();
        let b: std::sync::Arc<[u8]> = b"snapshot-b".as_slice().into();
        let c: std::sync::Arc<[u8]> = b"snapshot-c".as_slice().into();
        worker.submit(dir.join("w1.dat"), a.clone());
        worker.submit(dir.join("w2.dat"), b.clone()); // displaces w1
        worker.submit(dir.join("w3.dat"), c.clone()); // displaces w2
        worker.set_paused(false);
        worker.flush().unwrap();
        // Only the newest pending snapshot hit the disk.
        assert_eq!(worker.writes_completed(), 1);
        assert!(!dir.join("w1.dat").exists());
        assert!(!dir.join("w2.dat").exists());
        assert_eq!(std::fs::read(dir.join("w3.dat")).unwrap(), b"snapshot-c");
        // No stray temp files survive an atomic write.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // Sequential submissions each land.
        worker.submit(dir.join("w4.dat"), a.clone());
        worker.flush().unwrap();
        assert_eq!(std::fs::read(dir.join("w4.dat")).unwrap(), b"snapshot-a");

        // Drop writes the still-pending final save before joining.
        worker.submit(dir.join("w5.dat"), b.clone());
        drop(worker);
        assert_eq!(std::fs::read(dir.join("w5.dat")).unwrap(), b"snapshot-b");

        // Rotation: five backups, keep three -> newest three survive, monotonic names.
        let backup_dir = dir.join("backups");
        let mut paths = Vec::new();
        for i in 0..5u8 {
            paths.push(
                rotate_world_backups(&backup_dir, "world", &[i, i, i], 3).unwrap(),
            );
        }
        let mut names: Vec<String> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "world-00000003.bak".to_string(),
                "world-00000004.bak".to_string(),
                "world-00000005.bak".to_string()
            ]
        );
        assert_eq!(std::fs::read(&paths[4]).unwrap(), vec![4, 4, 4]);
        assert!(!paths[0].exists());
        assert!(!paths[1].exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `hasJoined` parsing through the injected transport: profile fields,
    /// 204 rejection, IP pin in the URL, malformed bodies.
    #[test]
    fn mojang_has_joined_transport_parses_profile_and_rejects() {
        let body = r#"{"id":"00112233445566778899aabbccddeeff","name":"Notch","properties":[{"name":"textures","value":"AA==","signature":"BB=="}]}"#;
        let seen_url = std::cell::RefCell::new(String::new());
        let profile = verify_mojang_has_joined_transport(
            "Notch",
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1",
            Some("203.0.113.7"),
            &|url| {
                *seen_url.borrow_mut() = url.to_string();
                Ok(Some(body.to_string()))
            },
        )
        .unwrap();
        assert_eq!(
            profile.uuid,
            unhex("00112233445566778899aabbccddeeff")[..]
        );
        assert_eq!(profile.name, "Notch");
        assert_eq!(profile.properties.len(), 1);
        assert_eq!(profile.properties[0].signature.as_deref(), Some("BB=="));
        let seen_url = seen_url.into_inner();
        assert!(seen_url.contains("username=Notch"));
        assert!(seen_url.contains("serverId=-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"));
        assert!(seen_url.contains("ip=203.0.113.7"));

        // 204 -> not joined.
        assert!(verify_mojang_has_joined_transport("Steve", "h", None, &|_| Ok(None)).is_err());
        // Transport error propagates.
        assert!(
            verify_mojang_has_joined_transport("Steve", "h", None, &|_| Err("offline".into()))
                .is_err()
        );
        // Malformed JSON / bad id both reject.
        for bad in [
            "not json",
            r#"{"id":"zz","name":"Steve"}"#,
            r#"{"id":"00112233445566778899aabbccddeeff"}"#,
        ] {
            let text = bad.to_string();
            assert!(
                verify_mojang_has_joined_transport("Steve", "h", None, &|_| Ok(Some(text.clone())))
                    .is_err(),
                "accepted {bad}"
            );
        }
    }

    // ---------- OPS-2: delta WAL + tick-fault quarantine ----------

    fn ops2_temp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-ops2-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ops2_block(x: i32, y: i32, z: i32, state: u32) -> DeltaWalRecord {
        DeltaWalRecord::BlockSet {
            dim: DimId::Overworld,
            x,
            y,
            z,
            state,
        }
    }

    /// Append -> checkpoint -> post-checkpoint writes: recovery replays only
    /// what the last marker did not already cover, and every record round-trips
    /// byte-exact.
    #[test]
    fn ops2_dwal_recovers_only_post_checkpoint_records_and_roundtrips_fields() {
        let dir = ops2_temp_dir("wal-replay");
        let path = dir.join("delta.wal");
        let block = ops2_block(12, -4, 700, 0x00ab_cdef);
        let snap = DeltaWalRecord::PlayerSnapshot {
            uuid: [0x5a; 16],
            dim: DimId::Nether,
            pos: [1.5, 64.25, -2.5],
            hp: 18.5,
            food: 17,
            xp: 123_456,
        };
        let slot = DeltaWalRecord::ContainerSlot {
            dim: DimId::End,
            x: -9,
            y: 70,
            z: 33,
            slot: 12,
            item_id: 881,
            count: 13,
        };
        {
            let mut wal = DeltaWalJournal::open(&path).unwrap();
            assert_eq!(wal.next_seq(), 1);
            assert_eq!(wal.append_and_flush(&block).unwrap(), 1);
            assert_eq!(wal.append_and_flush(&snap).unwrap(), 2);
            wal.append_checkpoint(41).unwrap();
            wal.append_and_flush(&slot).unwrap();
        }
        let report = recover_after_checkpoint(&path).unwrap();
        assert_eq!(report.checkpoint_seq, Some(41));
        assert_eq!(report.last_committed_seq, 4);
        assert_eq!(report.records, vec![slot.clone()]);
        assert_eq!(report.truncated_tail_bytes, 0);

        // A WAL with no marker replays every intact frame.
        let plain = dir.join("plain.wal");
        {
            let mut wal = DeltaWalJournal::open(&plain).unwrap();
            wal.append_and_flush(&block).unwrap();
            wal.append_and_flush(&snap).unwrap();
        }
        let report = recover_after_checkpoint(&plain).unwrap();
        assert_eq!(report.checkpoint_seq, None);
        assert_eq!(report.records, vec![block, snap]);

        // A missing WAL recovers as nothing to do.
        let report = recover_after_checkpoint(&dir.join("absent.wal")).unwrap();
        assert!(report.records.is_empty() && report.checkpoint_seq.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crash-torn trailing frame is reported and skipped while every prior
    /// CRC-verified frame stays intact; corrupt magic, CRC or length mid-stream
    /// fails closed.
    #[test]
    fn ops2_dwal_tolerates_torn_tail_and_fails_closed_on_corruption() {
        let dir = ops2_temp_dir("wal-torn");
        let path = dir.join("delta.wal");
        let r1 = ops2_block(1, 2, 3, 0xbeef);
        let r2 = ops2_block(4, 5, 6, 0xfeed);
        {
            let mut wal = DeltaWalJournal::open(&path).unwrap();
            wal.append_checkpoint(3).unwrap();
            wal.append_and_flush(&r1).unwrap();
            wal.append_and_flush(&r2).unwrap();
        }
        let intact_len = std::fs::metadata(&path).unwrap().len();

        // Simulate a crash mid-append: half a frame header at EOF.
        let torn = b"DWAL\x00\x00\x00";
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(torn)
            .unwrap();
        let report = recover_after_checkpoint(&path).unwrap();
        assert_eq!(report.truncated_tail_bytes, torn.len());
        assert_eq!(report.records, vec![r1.clone(), r2.clone()]);

        // Reopen truncates the torn tail so appends never sit behind garbage.
        {
            let mut wal = DeltaWalJournal::open(&path).unwrap();
            wal.append_checkpoint(4).unwrap();
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            intact_len + (DWAL_FRAME_MIN + 9) as u64 // tag(1) + u64 checkpoint_seq(8)
        );

        // A torn tail can also stop mid-payload: full header, partial record.
        let mut partial = Vec::new();
        partial.extend_from_slice(&DWAL_MAGIC);
        partial.extend_from_slice(&99u64.to_be_bytes());
        partial.extend_from_slice(&60u32.to_be_bytes()); // claims 60 payload bytes
        partial.extend_from_slice(&[7u8; 12]); // only 12 arrived
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&partial)
            .unwrap();
        let report = recover_after_checkpoint(&path).unwrap();
        assert_eq!(report.truncated_tail_bytes, partial.len());
        assert_eq!(report.checkpoint_seq, Some(4));
        assert!(report.records.is_empty());

        // Mid-stream corruption fails closed: flip one bit inside frame #1's
        // payload. The torn tail behind it is not what makes this an error.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[DWAL_HEADER_LEN + 5] ^= 0x40;
        std::fs::write(&path, &bytes).unwrap();
        let wal_err = recover_after_checkpoint(&path).unwrap_err();
        assert!(wal_err.contains("crc"), "{wal_err}");

        // Bad magic mid-stream fails closed too. Restore the payload bit, then
        // corrupt the first byte of frame #2's magic (frame #1 is a checkpoint
        // marker: tag(1) + u64(8) payload -> 16 + 9 + 4 = 29 bytes long).
        bytes[DWAL_HEADER_LEN + 5] ^= 0x40;
        bytes[DWAL_FRAME_MIN + 9] = b'X';
        std::fs::write(&path, &bytes).unwrap();
        let err = recover_after_checkpoint(&path).unwrap_err();
        assert!(err.contains("magic"), "{err}");

        // A forged length over the record cap fails closed even at EOF.
        let mut forged = Vec::new();
        forged.extend_from_slice(&DWAL_MAGIC);
        forged.extend_from_slice(&1u64.to_be_bytes());
        forged.extend_from_slice(&((MAX_WAL_RECORD_BYTES as u32) + 1).to_be_bytes());
        forged.extend_from_slice(&[0u8; 8]);
        std::fs::write(&path, &forged).unwrap();
        assert!(recover_after_checkpoint(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After a successful full save, the WAL shrinks to the fresh anchor marker
    /// atomically; sequence numbers keep climbing across the rotation.
    #[test]
    fn ops2_dwal_compact_after_checkpoint_truncates_and_keeps_monotonic_seq() {
        let dir = ops2_temp_dir("wal-compact");
        let path = dir.join("delta.wal");
        let rec = ops2_block(1, 2, 3, 5);
        let mut wal = DeltaWalJournal::open(&path).unwrap();
        wal.append_and_flush(&rec).unwrap(); // seq 1
        wal.append_checkpoint(10).unwrap(); // seq 2
        wal.append_and_flush(&rec).unwrap(); // seq 3
        wal.compact_after_checkpoint(10).unwrap(); // anchor = seq 4
        let report = recover_after_checkpoint(&path).unwrap();
        assert_eq!(report.checkpoint_seq, Some(10));
        assert!(report.records.is_empty());
        assert_eq!(report.last_committed_seq, 4);

        // Sequence stays monotonic across the rotation; the post-compaction
        // write is the only thing recovery replays.
        assert_eq!(wal.append_and_flush(&rec).unwrap(), 5);
        drop(wal);
        let report = recover_after_checkpoint(&path).unwrap();
        assert_eq!(report.records, vec![rec]);
        // No compact temp file is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Operator-controlled WAL and quarantine paths refuse `..`, NUL and
    /// control characters before any file is touched.
    #[test]
    fn ops2_wal_and_quarantine_paths_reject_traversal_and_control_chars() {
        let dir = ops2_temp_dir("wal-paths");
        assert!(DeltaWalJournal::open(dir.join("..").join("escape.wal")).is_err());
        assert!(DeltaWalJournal::open(dir.join("bad\nname.wal")).is_err());
        assert!(DeltaWalJournal::open(dir.join("nul\u{0}byte.wal")).is_err());
        assert!(recover_after_checkpoint(&dir.join("..").join("x.wal")).is_err());
        assert!(recover_after_checkpoint(&dir.join("tab\tname.wal")).is_err());
        assert!(DeltaWalJournal::open(dir.join("fine.wal")).is_ok());
        let q = TickFaultQuarantine::default();
        assert!(q.persist(&dir.join("..").join("q.txt")).is_err());
        assert!(TickFaultQuarantine::load_persisted(&dir.join("n\u{0}l.txt"), 1).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A panicking block-entity tick is caught without crashing the caller,
    /// frozen on first fault with a coordinate diagnostic, and skipped
    /// thereafter. Entity and chunk targets freeze independently.
    #[test]
    fn ops2_quarantine_freezes_panicking_target_and_skips_it() {
        let q = TickFaultQuarantine::default();
        let be = QuarantineTarget::BlockEntity {
            dim: DimId::Overworld,
            x: 12,
            y: 64,
            z: -5,
            kind: "minecraft:chest".into(),
        };
        let ran = AtomicU64::new(0);
        let outcome = run_quarantined_tick(&q, be.clone(), || {
            ran.fetch_add(1, Ordering::SeqCst);
            panic!("tick exploded\nwith \u{1}junk");
        });
        assert_eq!(outcome, TickFaultOutcome::Faulted { quarantined: true });
        assert!(q.is_quarantined(&be));
        let diag = q.diagnostic(&be).unwrap();
        assert!(diag.contains("minecraft:chest"), "{diag}");
        assert!(diag.contains("(12,64,-5)"), "{diag}");
        // The recorded line carries no control bytes or raw stack spam.
        assert!(!diag.chars().any(|c| c.is_control()), "{diag}");

        // Next tick for the same target: skipped, the closure never runs.
        let outcome = run_quarantined_tick(&q, be.clone(), || {
            ran.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(outcome, TickFaultOutcome::SkippedQuarantined);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        // Entity + chunk targets quarantine independently.
        let ent = QuarantineTarget::Entity {
            dim: DimId::Nether,
            eid: 77,
            kind: "minecraft:zombie".into(),
        };
        let outcome = run_quarantined_tick(&q, ent.clone(), || panic!("mob boom"));
        assert_eq!(outcome, TickFaultOutcome::Faulted { quarantined: true });
        assert!(q.is_quarantined(&ent));
        let chunk = QuarantineTarget::Chunk {
            dim: DimId::End,
            cx: -3,
            cz: 8,
        };
        assert!(!q.is_quarantined(&chunk));
        assert_eq!(
            run_quarantined_tick(&q, chunk, || {}),
            TickFaultOutcome::Ticked
        );
        assert_eq!(
            q.quarantined()
                .iter()
                .filter(|t| **t != ent)
                .collect::<Vec<_>>(),
            vec![&be]
        );
    }

    /// `unfreeze` refuses operator levels below 4 and only level 4 releases;
    /// a higher fault threshold needs that many consecutive faults to freeze,
    /// and one clean tick resets the run.
    #[test]
    fn ops2_quarantine_unfreeze_requires_op_level_4_and_threshold_resets() {
        let q = TickFaultQuarantine::new(3);
        let chunk = QuarantineTarget::Chunk {
            dim: DimId::Overworld,
            cx: 0,
            cz: 0,
        };
        let boom = || panic!("chunk tick died");
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), boom),
            TickFaultOutcome::Faulted { quarantined: false }
        );
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), boom),
            TickFaultOutcome::Faulted { quarantined: false }
        );
        assert!(!q.is_quarantined(&chunk));
        // A clean tick resets the consecutive-fault counter.
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), || {}),
            TickFaultOutcome::Ticked
        );
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), boom),
            TickFaultOutcome::Faulted { quarantined: false }
        );
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), boom),
            TickFaultOutcome::Faulted { quarantined: false }
        );
        assert_eq!(
            run_quarantined_tick(&q, chunk.clone(), boom),
            TickFaultOutcome::Faulted { quarantined: true }
        );
        assert!(q.is_quarantined(&chunk));

        // Only op level 4 may release a frozen crash source.
        for level in [0u8, 1, 2, 3] {
            assert!(q.unfreeze(&chunk, level).is_err(), "level {level}");
            assert!(q.is_quarantined(&chunk), "level {level} released it");
        }
        assert_eq!(q.unfreeze(&chunk, 4), Ok(true));
        assert!(!q.is_quarantined(&chunk));
        assert_eq!(q.unfreeze(&chunk, 4), Ok(false));
        let other = QuarantineTarget::Chunk {
            dim: DimId::Overworld,
            cx: 9,
            cz: 9,
        };
        assert_eq!(q.unfreeze(&other, 4), Ok(false));
        // After release the target ticks again.
        assert_eq!(
            run_quarantined_tick(&q, chunk, || {}),
            TickFaultOutcome::Ticked
        );
    }

    /// The frozen set round-trips through the persisted evidence file so a
    /// crash-looping target stays frozen across a restart.
    #[test]
    fn ops2_quarantine_persist_roundtrip_keeps_targets_frozen() {
        let dir = ops2_temp_dir("quar-persist");
        let path = dir.join("quarantine.txt");
        let q = TickFaultQuarantine::default();
        let be = QuarantineTarget::BlockEntity {
            dim: DimId::Overworld,
            x: 1,
            y: 2,
            z: 3,
            kind: "minecraft:hopper".into(),
        };
        let ent = QuarantineTarget::Entity {
            dim: DimId::Nether,
            eid: 42,
            kind: "minecraft:piglin".into(),
        };
        run_quarantined_tick(&q, be.clone(), || panic!("be died"));
        run_quarantined_tick(&q, ent.clone(), || panic!("ent died"));
        q.persist(&path).unwrap();
        let loaded = TickFaultQuarantine::load_persisted(&path, 1).unwrap();
        assert!(loaded.is_quarantined(&be));
        assert!(loaded.is_quarantined(&ent));
        let diag = loaded.diagnostic(&be).unwrap();
        assert!(diag.contains("minecraft:hopper"), "{diag}");
        assert!(!diag.contains('\n'), "{diag}");
        // A missing file loads as an empty quarantine.
        let fresh = TickFaultQuarantine::load_persisted(&dir.join("absent.txt"), 1).unwrap();
        assert!(!fresh.is_quarantined(&be));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- per-chunk lag hotspot quarantine ----

    /// One cost sample per tick for `ticks` consecutive ticks.
    fn hotspot_ticks(
        q: &ChunkHotspotQuarantine,
        chunk: (DimId, i32, i32),
        kind: ChunkCostKind,
        cost_us: u64,
        first_tick: u64,
        ticks: u64,
    ) {
        let (dim, cx, cz) = chunk;
        for tick in first_tick..first_tick + ticks {
            q.record_chunk_cost_us(dim, cx, cz, kind, cost_us, tick);
        }
    }

    /// A sustained 20 ms mod machine quarantines its own chunk — and only its
    /// own chunk — dropping to one tick in eight while the healthy neighbor
    /// keeps every tick.
    #[test]
    fn hotspot_quarantine_throttles_only_the_hot_chunk() {
        let q = ChunkHotspotQuarantine::new();
        hotspot_ticks(&q, (DimId::Overworld, 0, 0), ChunkCostKind::Entities, 500, 0, 8);
        // Four samples: the third completed tick (scored on the fourth
        // record) crosses `trigger_ticks` and engages the quarantine.
        hotspot_ticks(&q, (DimId::Nether, 10, -4), ChunkCostKind::ModMachine, 20_000, 0, 4);
        assert!(q.is_chunk_quarantined(DimId::Nether, 10, -4));
        assert!(!q.is_chunk_quarantined(DimId::Overworld, 0, 0));
        for tick in 0..16u64 {
            assert!(
                q.chunk_due(DimId::Overworld, 0, 0, tick),
                "healthy neighbor must tick every tick ({tick})"
            );
            // Quarantine engaged while scoring completed tick 2, so the
            // chunk owns phase slot 2 of every 8-tick window.
            assert_eq!(
                q.chunk_due(DimId::Nether, 10, -4, tick),
                tick % 8 == 2,
                "throttled chunk cadence at tick {tick}"
            );
        }
        let alerts = q.alerts();
        assert!(
            alerts.iter().any(|a| a.contains("quarantined")
                && a.contains("(10,-4)")
                && a.contains("mod_machine")
                && a.contains("minecraft:the_nether")),
            "{alerts:?}"
        );
    }

    /// One giant spike is smoothed by the EWMA and never reaches
    /// `trigger_ticks` consecutive hot ticks.
    #[test]
    fn hotspot_single_spike_does_not_quarantine() {
        let q = ChunkHotspotQuarantine::new();
        q.record_chunk_cost_us(DimId::End, 7, 7, ChunkCostKind::ModMachine, 60_000, 0);
        hotspot_ticks(&q, (DimId::End, 7, 7), ChunkCostKind::ModMachine, 500, 1, 6);
        assert!(!q.is_chunk_quarantined(DimId::End, 7, 7));
    }

    /// Hysteresis: the quarantine lifts itself only after
    /// `hysteresis_exit_ticks` consecutive cool completed ticks, and the lift
    /// is recorded as an operator alert.
    #[test]
    fn hotspot_hysteresis_auto_lifts_after_cool_streak() {
        let q = ChunkHotspotQuarantine::new();
        hotspot_ticks(&q, (DimId::Overworld, 3, 3), ChunkCostKind::ModMachine, 20_000, 0, 4);
        assert!(q.is_chunk_quarantined(DimId::Overworld, 3, 3));
        // Every cheap completed tick counts toward the 20-tick cool streak:
        // records through tick 20 score completed ticks 3..19 (16 cool), so
        // the chunk is still quarantined; records through tick 30 score
        // through tick 29, and the twentieth cool score lands on completed
        // tick 23 — the lift fires mid-window.
        for tick in 4..=20u64 {
            q.record_chunk_cost_us(DimId::Overworld, 3, 3, ChunkCostKind::ModMachine, 1_000, tick);
        }
        assert!(q.is_chunk_quarantined(DimId::Overworld, 3, 3));
        for tick in 21..=30u64 {
            q.record_chunk_cost_us(DimId::Overworld, 3, 3, ChunkCostKind::ModMachine, 1_000, tick);
        }
        assert!(!q.is_chunk_quarantined(DimId::Overworld, 3, 3));
        assert!(q.chunk_due(DimId::Overworld, 3, 3, 999));
        assert!(
            q.alerts().iter().any(|a| a.contains("auto-lifted")),
            "{:?}",
            q.alerts()
        );
    }

    /// An operator clear lifts the quarantine immediately and is logged;
    /// clearing a healthy chunk is a no-op.
    #[test]
    fn hotspot_manual_clear_lifts_quarantine() {
        let q = ChunkHotspotQuarantine::new();
        hotspot_ticks(&q, (DimId::Overworld, 8, 8), ChunkCostKind::Entities, 25_000, 0, 4);
        assert!(q.is_chunk_quarantined(DimId::Overworld, 8, 8));
        assert!(!q.clear_chunk_quarantine(DimId::Overworld, 9, 9));
        assert!(q.clear_chunk_quarantine(DimId::Overworld, 8, 8));
        assert!(!q.is_chunk_quarantined(DimId::Overworld, 8, 8));
        assert!(q.chunk_due(DimId::Overworld, 8, 8, 3));
        assert!(
            q.alerts()
                .iter()
                .any(|a| a.contains("cleared by operator") && a.contains("(8,8)")),
            "{:?}",
            q.alerts()
        );
        // A second clear finds nothing to lift.
        assert!(!q.clear_chunk_quarantine(DimId::Overworld, 8, 8));
    }

    /// The tracked set is hard-bounded: past `CHUNK_HOTSPOT_MAX_TRACKED` the
    /// least-recently-active entry evicts, and refreshing a chunk protects it.
    #[test]
    fn hotspot_map_is_bounded_and_evicts_least_recently_active() {
        let q = ChunkHotspotQuarantine::new();
        for cx in 0..CHUNK_HOTSPOT_MAX_TRACKED as i32 {
            q.record_chunk_cost_us(DimId::Overworld, cx, 0, ChunkCostKind::Entities, 100, 0);
        }
        // Refresh chunk cx=0 so it is not the oldest idle entry.
        q.record_chunk_cost_us(DimId::Overworld, 0, 0, ChunkCostKind::Entities, 100, 9);
        // One more distinct chunk forces exactly one eviction.
        q.record_chunk_cost_us(DimId::Overworld, CHUNK_HOTSPOT_MAX_TRACKED as i32, 0, ChunkCostKind::Entities, 100, 10);
        let top = q.top_lag_hotspots(usize::MAX);
        assert_eq!(top.len(), CHUNK_HOTSPOT_MAX_TRACKED);
        assert!(top.iter().any(|s| s.cx == 0 && s.cz == 0));
        // cx=1 held the oldest activity stamp and the smallest key: evicted.
        assert!(!top.iter().any(|s| s.cx == 1 && s.cz == 0));
        assert!(top.iter().any(|s| s.cx == CHUNK_HOTSPOT_MAX_TRACKED as i32));
    }

    /// `top_lag_hotspots` orders by smoothed cost descending with a
    /// deterministic tie-break, and `format_hotspot_report` renders the
    /// Spark-free `/megu status` diagnosis from the same ordering.
    #[test]
    fn hotspot_top_lag_hotspots_orders_and_report_formats() {
        let q = ChunkHotspotQuarantine::new();
        hotspot_ticks(&q, (DimId::Overworld, 1, 1), ChunkCostKind::ModMachine, 30_000, 0, 4);
        hotspot_ticks(&q, (DimId::Nether, 2, 2), ChunkCostKind::Entities, 10_000, 0, 3);
        hotspot_ticks(&q, (DimId::End, 3, 3), ChunkCostKind::BlockEntity, 2_000, 0, 3);
        hotspot_ticks(&q, (DimId::Overworld, 4, 4), ChunkCostKind::Redstone, 100, 0, 3);
        let top = q.top_lag_hotspots(10);
        let coords: Vec<(DimId, i32, i32)> = top.iter().map(|s| (s.dim, s.cx, s.cz)).collect();
        assert_eq!(
            coords,
            vec![
                (DimId::Overworld, 1, 1),
                (DimId::Nether, 2, 2),
                (DimId::End, 3, 3),
                (DimId::Overworld, 4, 4),
            ]
        );
        assert!(top[0].quarantined);
        assert_eq!(top[0].dominant_kind, ChunkCostKind::ModMachine);
        assert_eq!(top[0].ewma_us, 30_000);
        assert_eq!(top[3].dominant_kind, ChunkCostKind::Redstone);
        assert!(!top[3].quarantined);

        let report = q.format_hotspot_report();
        assert!(report.contains("chunk hotspot report: 4 chunks tracked, 1 quarantined"));
        assert!(report.contains("chunk=(1, 1)"), "{report}");
        assert!(report.contains("kind=mod_machine"), "{report}");
        assert!(report.contains("QUARANTINED"), "{report}");
        assert!(report.contains("recent alerts:"), "{report}");
        assert!(report.contains("[hotspot] quarantined"), "{report}");
    }
}
