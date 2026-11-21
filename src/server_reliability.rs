// ---------- real-world server must-haves ----------
//
// The four pieces a public server cannot ship without, kept dependency-free so the
// audited crate set in `Cargo.toml` does not change:
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
//
// Nothing here reads a wall clock: save/backup naming comes from a monotonic
// sequence drawn from the directory itself, which survives restarts.

// ================= AES-128 (FIPS-197) in CFB8 mode =================
//
// Minecraft encrypts login onward with AES/CFB8/NoPadding: a 128-bit block cipher
// whose keystream byte 0 is XORed into one data byte at a time, with the produced
// ciphertext byte shifted into the 16-byte feedback register. Only the forward
// cipher is ever needed - decryption reuses it.

#[allow(dead_code)]
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
#[allow(dead_code)]
const AES_RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// GF(2^8) doubling modulo the AES polynomial.
#[allow(dead_code)]
fn aes_xtime(x: u8) -> u8 {
    if x & 0x80 != 0 {
        (x << 1) ^ 0x1b
    } else {
        x << 1
    }
}

/// FIPS-197 §5.2 key expansion: 11 round keys of 16 bytes for AES-128.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) struct Aes128Cfb8 {
    round_keys: [u8; 176],
    feedback: [u8; 16],
}

#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) fn mc_server_hash(server_id: &str, shared_secret: &[u8], public_key_der: &[u8]) -> String {
    let mut data = Vec::with_capacity(server_id.len() + shared_secret.len() + public_key_der.len());
    data.extend_from_slice(server_id.as_bytes());
    data.extend_from_slice(shared_secret);
    data.extend_from_slice(public_key_der);
    mc_signed_hex_digest(&sha1(&data))
}

// ================= HMAC-SHA256 (RFC 2104) =================

/// RFC 2104 HMAC over the existing FIPS 180-4 SHA-256 primitive.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[allow(dead_code)]
pub(crate) const LOGIN_ENCRYPTION_REQUEST_ID: i64 = 0x01;
#[allow(dead_code)]
pub(crate) const LOGIN_ENCRYPTION_RESPONSE_ID: i64 = 0x01;

/// Protocol byte array: VarInt length then that many bytes.
#[allow(dead_code)]
fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len() as i64);
    out.extend_from_slice(bytes);
}

/// Bounded read of a protocol byte array; `max` stops a forged length.
#[allow(dead_code)]
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
#[allow(dead_code)]
fn read_string_bounded(b: &mut &[u8], max: usize) -> Option<String> {
    let bytes = read_byte_array(b, max)?;
    String::from_utf8(bytes).ok()
}

/// The body of S2C Login `0x01` (packet id is written by `send_packet`).
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForwardedProperty {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) signature: Option<String>,
}

/// The verified contents of a Velocity modern-forwarding payload.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VelocityForwardedPlayer {
    pub(crate) version: i32,
    pub(crate) forwarded_ip: String,
    pub(crate) uuid: [u8; 16],
    pub(crate) username: String,
    pub(crate) properties: Vec<ForwardedProperty>,
}

#[allow(dead_code)]
pub(crate) const VELOCITY_FORWARDING_CHANNEL: &str = "velocity:player_info";
#[allow(dead_code)]
pub(crate) const VELOCITY_FORWARDING_VERSION_MAX: i32 = 4;

/// Verify and decode a `velocity:player_info` payload. Errors are deliberately
/// undifferentiated enough to log without giving a forger an oracle.
#[allow(dead_code)]
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
    let uuid: [u8; 16] = b[..16].try_into().unwrap();
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
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MojangJoinedProfile {
    pub(crate) uuid: [u8; 16],
    pub(crate) name: String,
    pub(crate) properties: Vec<ForwardedProperty>,
}

/// Undashed 32-hex `id` field -> raw 16-byte UUID.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
                .read_to_string()
                .map(Some)
                .map_err(|e| format!("sessionserver read failed: {e}"))
        }
        Err(ureq::Error::StatusCode(204)) => Ok(None),
        Err(e) => Err(format!("sessionserver request failed: {e}")),
    }
}

/// Parse the `hasJoined` JSON body: `{id, name, properties:[{name,value,signature?}]}`.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) static ISOLATED_CONNECTION_DISCONNECTS: AtomicU64 = AtomicU64::new(0);

/// Pull a readable message out of a panic payload for the operator log.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[allow(dead_code)]
pub(crate) enum ConnStream {
    Plain(TcpStream),
    Encrypted {
        stream: TcpStream,
        rx: Aes128Cfb8,
        tx: Aes128Cfb8,
    },
}

#[allow(dead_code)]
impl ConnStream {
    /// The socket underneath either lane, for timeouts, shutdown and peer
    /// lookups the transport enum itself does not model.
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            ConnStream::Plain(s) => s,
            ConnStream::Encrypted { stream, .. } => stream,
        }
    }

    /// Switch a still-plain transport to encrypted with the negotiated shared
    /// secret (key == IV, per the Java protocol). Fails on an already-encrypted
    /// stream rather than silently restarting the keystream.
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

#[allow(dead_code)]
impl From<TcpStream> for ConnStream {
    fn from(stream: TcpStream) -> Self {
        ConnStream::Plain(stream)
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
// The login handshake needs two things the audited dependency set cannot grow
// into this file: an RSA keypair (the Encryption Request's public key, and the
// private key that unwraps the client's answer) and the session-server check.
// Both sit behind this installed-at-startup provider so the handshake stays
// testable offline and the keypair can arrive as a later lane without a
// signature change. `None` (the shipping default) fails every online-mode
// login closed rather than pretending to verify.

/// The keypair and session-verification half of online-mode login.
#[allow(dead_code)]
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

fn online_auth_slot() -> &'static Mutex<Option<Arc<dyn OnlineAuth>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<dyn OnlineAuth>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or clear) the online-mode provider. Startup wiring and tests use
/// this; the login path reads through `online_auth_provider`.
#[allow(dead_code)]
pub(crate) fn set_online_auth_provider(provider: Option<Arc<dyn OnlineAuth>>) {
    *online_auth_slot().lock().unwrap() = provider;
}

/// The installed provider, or `None` when online mode must fail closed.
#[allow(dead_code)]
pub(crate) fn online_auth_provider() -> Option<Arc<dyn OnlineAuth>> {
    online_auth_slot().lock().unwrap().clone()
}

// ================= async background autosave =================
//
// The tick loop produces an immutable serialized snapshot and hands it to one
// writer thread through a single slot: if the disk is still busy when the next
// snapshot is ready the pending one is *replaced*, never queued, so memory stays
// bounded no matter how slow the disk gets. Every write is temp-file + sync +
// rename so a crash can leave the old or the new file, never a torn one.

/// One unit of save work: the target path and the already-serialized bytes.
#[allow(dead_code)]
pub(crate) struct SaveJob {
    pub(crate) path: std::path::PathBuf,
    pub(crate) bytes: std::sync::Arc<[u8]>,
    pub(crate) seq: u64,
}

#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) struct AsyncSaveWorker {
    state: std::sync::Arc<(Mutex<SaveWorkerState>, std::sync::Condvar)>,
    next_seq: AtomicU64,
    thread: Option<thread::JoinHandle<()>>,
}

#[allow(dead_code)]
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
            .name("mc-spine-save".into())
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
                let mut g = lock.lock().unwrap();
                loop {
                    if !g.paused || g.closing {
                        if let Some(job) = g.pending.take() {
                            break job;
                        }
                    }
                    if g.closing {
                        return;
                    }
                    g = cv.wait(g).unwrap();
                }
            };
            let result = write_bytes_atomic(&job.path, &job.bytes);
            let mut g = lock.lock().unwrap();
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
    pub(crate) fn submit(&self, path: impl Into<std::path::PathBuf>, bytes: std::sync::Arc<[u8]>) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*self.state;
        lock.lock().unwrap().pending = Some(SaveJob {
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
        let mut g = lock.lock().unwrap();
        while g.last_done < target {
            g = cv.wait(g).unwrap();
        }
        match g.last_error.take() {
            Some(message) => Err(std::io::Error::new(std::io::ErrorKind::Other, message)),
            None => Ok(()),
        }
    }

    /// Completed writes, for tests and operator telemetry.
    pub(crate) fn writes_completed(&self) -> u64 {
        self.state.0.lock().unwrap().writes
    }

    /// Pause/resume draining. Used by tests to force coalescing deterministically;
    /// shutdown still drains a pending job regardless.
    #[cfg(test)]
    pub(crate) fn set_paused(&self, paused: bool) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap().paused = paused;
        cv.notify_all();
    }
}

impl std::ops::Drop for AsyncSaveWorker {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap().closing = true;
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
#[allow(dead_code)]
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
    existing.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in existing.into_iter().skip(max_keep) {
        let _ = std::fs::remove_file(old);
    }
    Ok(path)
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
            "mc-spine-reliability-{}-{}",
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
}
